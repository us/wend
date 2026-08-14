# What `wend recall` does and does not do — measured

Five harnesses in this directory, all run against one real 757-session corpus.
They exist mainly to preserve a **negative** result, because the tempting way to
use this feature is the way that does not work.

Reproduce: build the case library (`wend index --embed` with the Azure backend),
then `uv run python evals/eval_recall.py`. The scripts read a copy of the index
and delete held-out sessions from the copy; they never write to a real index.

## 1. Predicting the user's stance — does not work

`eval_recall.py`. Leave-one-**session**-out (120 sessions held out, 2,594 cases
removed, 5,443 left in the library), 500 probes, 477 answered. Graded by whether
the retrieved set contains a precedent whose stance matches what the user
actually said, against drawing the same number of cases at random.

| metric | retrieval | random | lift | 95% CI | McNemar |
|---|---:|---:|---:|---|---|
| any of 5 matches | 88.1% | 90.8% | −2.7% | 84.8–90.7% | p≈0.49 |
| majority vote | 35.8% | 43.4% | −7.5% | 31.7–40.3% | p≈0.06 |
| strongest precedent | 35.2% | 41.9% | −6.7% | 31.1–39.6% | p≈0.14 |

Retrieval is **not better than random** at this, and trends worse. The
differences are not significant, but "no better than random" is established.

**Why it is worse, and it is by design.** Actual stance mix over the probes was
neutral 246 / approve 170 / reject 84 — rejection is 17%. The retrieved mix was
approve 892 / reject 863 / neutral 630 — rejection is 36%, more than double.
That skew is produced deliberately by the diagnosticity term, which upweights
emphatic and rule-stating reactions — disproportionately rejections. It exists to
stop the system rubber-stamping. It works, and in doing so it wrecks the retrieved
distribution as a *predictor*.

(This run predates two later changes: the stance classifier and its polarity
quota were removed for keying off hand-written Turkish and English word lists,
and near-duplicate suppression is now character-trigram based. The skew and the
conclusion survive both — diagnosticity alone produces it.)

Teaching what standards the user applies and predicting what they will say are
different objectives, and optimising the first degrades the second.

**An earlier run of this same eval used 55 probes and reported "zero lift" as if
it were decisive.** At n=55 the 95% interval is ±12.7pp — the result was
indistinguishable from noise in either direction. The 500-probe run is the one to
cite.

## 2. Is stance predictable at all — no

`ceiling.py`. Same held-out probes, asking a frontier-class model (DeepSeek-V4-Pro)
to predict the stance directly. This bounds every training question: if a strong
model cannot beat a constant guess, no smaller model, classifier or fine-tune
can, because the information is not in the input.

| condition | accuracy |
|---|---:|
| always guess the majority class | **56.0%** |
| LLM, situation only | 34.0% |
| LLM + the user's own retrieved precedents | 29.5% |

Nothing beats the constant. Worse, the model does not read the situation at all —
it picks a prior and applies it. Prompted with a persona sketch ("blunt, demands
proof, pushes back") it answered *reject* on 40 of 50 probes against a true rate
of 14%; with the persona removed it flipped to *approve* on 39 of 50. The
caricature, not the situation, drove the answer.

The reason is structural: the same situation text ("finished X, tests pass,
shall I proceed?") precedes both approval and rejection depending on whether the
work was actually good — and work quality is not in the indexed text.

## 3. What does work — surfacing the right criteria

`overlap.py`. Instead of asking whether the polarity matches, ask whether the
retrieved reactions share vocabulary with what the user actually said — the
criteria they raise (test it, prove it, what did it cost, show me).

| | best-of-5 token overlap |
|---|---:|
| retrieval | 0.0864 |
| random | 0.0520 |
| **lift** | **1.66×** |

This is the signal the feature actually carries, and it is what the output should
be read as: *these are the standards this person has applied in moments like
this*, never *this is what they would say*.

## Consequences

- `recall` output is evidence about the past, not a prediction, and never a
  substitute for the user's approval.
- Do not train a stance classifier, reward model, DPO policy or LoRA on
  (situation → stance). The label is not a function of the feature.
- If verdict prediction is ever wanted, the missing feature is artifact quality,
  which is recoverable from the transcript post-hoc: did the tests pass, was the
  change reverted in the next commit, how many correction turns followed, did the
  user re-ask the same thing. That is a data problem, not a model problem.

## Caveat that applies to all three

Ground-truth stance comes from a keyword classifier that agrees with hand labels
about 57% of the time and systematically misses implicit rejection. Some of every
gap above is label noise rather than retrieval failure. The direction of the
conclusions survives that, but the exact percentages should not be quoted as
precise.

## Running these

The harnesses need a built `wend` with the Azure backend, a populated case
library, and somewhere to keep their working copies of the index:

```bash
export WEND_EVAL_DIR=~/.cache/wend-evals     # working dir; defaults to this
export WEND_BIN=./target/release/wend        # defaults to `wend` on PATH
mkdir -p "$WEND_EVAL_DIR"
cp ~/.local/share/wend/index.db "$WEND_EVAL_DIR/test-index.db"
printf '%s' "$WEND_AZURE_ENDPOINT" > "$WEND_EVAL_DIR/ep.txt"
printf '%s' "$WEND_AZURE_KEY"      > "$WEND_EVAL_DIR/key.txt"

uv run python evals/eval_recall.py   # builds the held-out split the others reuse
uv run python evals/ceiling.py
uv run python evals/overlap.py
uv run python evals/replicate.py
uv run python evals/pattern.py
```

`eval_recall.py` must run first: it writes `eval-index.db`, the copy with whole
sessions removed that the others probe against. Nothing here writes to a real
index, and the API key is read from a file rather than baked into a script.
