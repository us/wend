# Plan v2: `wend recall` — situation-conditioned case retrieval

Answer "what would this user say here?" by retrieving what they actually said in
the most similar past situations, instead of distilling their history into a
static rulebook.

Revised after review by Codex, `pr-rust`, `pr-ml` and `pr-lazy` (13 criticals
between them). Corrections from v1 are marked ✎.

## Why, and what the evidence says

The existing `profile` skill mines the corpus into a `CLAUDE.md` block of
standing rules. That block is accurate and useful but structurally cannot do
this job: it keeps only what *recurs broadly*, which is the filter that deletes
the rare emphatic correction, and it is static, so it cannot condition on the
situation at hand.

**Measured on this user's real corpus** (`scratchpad/res-{theory,oss,data}.md`):

| fact | number |
|---|---|
| ✎ prose reaction pairs in the corpus (res-data's funnel) | **7,779** |
| of a hand-labelled sample of 120, genuinely *responsive* to the preceding turn | **73.3%** |
| pivots to a new topic (unrecoverable by any situation representation) | 24.2% (only ~5% fully unrelated) |
| classes carrying transferable judgment (reject / skeptical / nudge) | 33.4% |
| cases after filtering junk and duplicates | **6,982** |
| situation size / one-time cost | 413 tok → **$0.375** |

✎ v1 headlined "9,932 (83%)" from a looser SQL filter than the one res-data
actually funnelled from. 7,779 is the number that matches the funnel, the class
percentages and the cost model. The looser figure is not used anywhere.

**Prototype, live endpoint, 1,200 cases:** situation-conditioned retrieval does
surface behavioural patterns across unrelated topics — a "we verified it all in
prod" claim retrieved his past *"test ettin mi benim api key ile?"*. But pure
cosine also returned topically-adjacent, behaviourally-wrong cases; a Cohere
rerank pass over candidate *situations* fixed 3 of 4 visible cases.

**Highest-confidence external result** (controlled ablation, arXiv:2601.00821):
verbatim chunks beat LLM-extracted artifacts by +15.9 / +22.0 points, and the gap
persists when the extraction is a semantic graph. Derived structure sits
*alongside* raw text, never replacing it.

## Design

### 1. Storage ✎ — extend `chunks`, do not add parallel tables

v1 proposed `cases` + `case_vectors` and claimed this "reuses the existing model
guard, batch writer and backfill loop". All four reviewers independently showed
that claim is false: `insert_session_chunks`, `chunks_needing_vectors`,
`store_chunk_vectors_batch`, `chunk_vector_count` and `all_chunk_vectors`
(`store/mod.rs:533-637`) each embed `chunks`/`chunk_vectors` literally in hand-written
SQL, and `build_chunks`/`embed_pending` (`embed/mod.rs:287-327`) call them by
name. Parallel tables mean **rewriting all five with `chunk`→`case` substituted**.

So schema **v5 adds two nullable columns to `chunks`** instead:

```sql
ALTER TABLE chunks ADD COLUMN kind TEXT NOT NULL DEFAULT 'prose';
ALTER TABLE chunks ADD COLUMN payload TEXT;      -- verbatim reaction, cases only
ALTER TABLE chunks ADD COLUMN src_message_fk INTEGER;  -- ✎ provenance: the reaction's row
```

A case is a chunk whose `text` is the situation and whose `payload` is the user's
verbatim reaction. The *embedding* pipeline then works unmodified — same model
guard, same batch writer, same resume logic — because it only ever reads
`chunks.text`.

✎ **`src_message_fk` is required, not decoration.** v2 promised `recall` would
print a date, but `chunks` has no `ts`; the timestamp lives on `messages`. This
column also gives every case provenance back to the exact transcript line.

✎ Two changes to *existing* queries are unavoidable, and v2 wrongly claimed the
pipeline was untouched:

1. `all_chunk_vectors` and `chunks_needing_vectors` take a `kind`. ✎ The complete
   caller list, since a missed one strands a whole kind:
   `hybrid_search` (`embed/mod.rs:356`) → **prose only**;
   `recall` (new) → **cases only**;
   `embed_pending` (`embed/mod.rs:302`) and the cost estimate in `run_embed`
   (`wend-cli/src/main.rs:399`) → **both kinds**, or one of them never gets
   embedded and never gets paid for.

   ✎ `ChunkVec` is also insufficient for `recall`: it carries neither `payload`,
   `src_message_fk`, the chunk id, nor the reaction's timestamp. `recall` needs
   its own joined accessor returning those, rather than a `kind` parameter bolted
   onto a struct shaped for prose search.
2. **`sessions_without_chunks` must become kind-aware** (`store/mod.rs:451`). It
   currently treats *any* chunk row as proof that prose chunking is finished, so
   inserting a session's cases first would permanently suppress its prose chunks —
   silently shrinking ordinary semantic search. Case building needs the mirror
   check for its own idempotency.

✎ `idx_chunks_kind` dropped: two low-selectivity values over ~30k rows will not
beat a scan, and it costs write throughput on every insert. Add one only if
`EXPLAIN QUERY PLAN` says otherwise.

✎ **The three `ALTER`s and the version bump must sit inside an explicit
`BEGIN … COMMIT`.** `execute_batch` is not atomic on its own — it hands the whole
string to SQLite, where each DDL statement autocommits — so an interruption after
the first `ALTER` leaves the DB at v4 with one column added, and the retry dies
on "duplicate column name" with no `IF NOT EXISTS` available for
`ALTER TABLE ADD COLUMN`. v3 dodged this with `IF NOT EXISTS` on `CREATE TABLE`
(`schema.rs:28`); v5 has no such escape and needs the transaction.

Additive and non-destructive — unlike v4 nothing is deleted and no chunk is
re-embedded.

✎ **No `klass` column.** v1 stored a reaction class from a classifier measured at
**56.7%** agreement with hand labels and *systematically* biased (it misses
rejection and skepticism because they are usually implicit). Pinning a coin flip
into the schema means three separate mechanisms inherit the bias and fixing the
classifier needs a migration. Classes are recomputed on read; it is a regex over
one short string.

✎ **No `corrected` column in v1 of this feature.** It has no consumer yet (see
§5), and a column with no reader is speculative.

### 2. The situation representation ✎

```
situation = prev_user_chars[:600] + "\n" + assistant_prose_chars[-1000:] + "\n" + tool_signature
tool_signature = "Edit×11 Bash×4 Agent×3"     ✎ names with counts, stable order
```

✎ **Character slices, not byte slices.** v1 said "byte caps"; the measurements
behind 413 tokens were character slices, and raw byte slicing panics on Turkish
UTF-8 — this repo just fixed exactly that bug class in `chunk_texts`
(`embed/mod.rs:228`). Slice on char boundaries.

✎ **Counts, not just unique names.** `Edit×11` and `Edit×1` are materially
different agent actions; `uniq()` collapses them into the same signature.

✎ **A new raw-turn assembler is required — no existing accessor can produce
this.** `semantic_messages` (`store/mod.rs:466`) returns user rows only.
`list_prose_messages` (`:489`) filters `content_json LIKE '[{"kind":"text"%'`, and
`Block` serializes with `#[serde(tag="kind")]` (`model.rs:17`), so **any assistant
turn whose first block is `Thinking` or `ToolUse` is dropped** — precisely the
tool-heavy turns whose prose tail and tool names we want. Tool names exist only
inside `content_json` and no current query surfaces them.

The assembler therefore walks raw `messages` ordered by `line_no` and:
- treats a **spoken** user row (first block `text`, not `<…>`, not `/…`) as the
  reaction and as the turn boundary;
- walks backwards over every assistant row, concatenating `text` blocks and
  collecting `tool_use` names;
- **does not treat user `tool_result` rows as boundaries** — they carry
  `role='user'` but are not the user speaking, and a tool-heavy turn interleaves
  them with assistant rows;
- ✎ **skips `is_sidechain` rows.** Subagent turns are stored in the same table
  and no raw accessor exposes the flag (`store/mod.rs:412`), so walking rows
  naively splices subagent conversation into a main-thread case — attributing to
  the user a situation they never saw.

✎ "Spoken" needs three more exclusions, all observed in the prototype's output:
`is_compact_summary` rows (a system-generated continuation, not the user);
bracketed system markers such as `[Request interrupted by user]`, which the
prototype dutifully retrieved as if it were a reaction; and `[Image #N]`-only
messages, which carry no text signal at all.

### 2b. Build-time filtering ✎

The 6,982-case count and the $0.375 cost both assume the corpus is filtered
*before* embedding, which v2 never said. The funnel, from res-data's 7,779:
drop reactions under 8 chars ("ee", "go"), over 2,000 chars (pasted logs), the
noise class, and **exact normalised duplicates** — keeping the *earliest*
occurrence, since the first time he said it is the one that was not a repeat.
Retrieval-time dedup (§4.3) is then a second, cheaper guard against duplicates
that survive normalisation, not the primary mechanism.

Rejected: generating a per-turn summary. 91 MB through a model, ~50× the
embedding cost, to derive what two free components already give.

### 3. Scoring ✎ — relevance is not enough

v1's four stages all scored *relevance* and nothing else, which reproduces the
exact failure the prototype measured: across 12k messages "tamam"/"devam" are
numerous and embed near everything, while a single furious *"bir daha asla
onayım olmadan commit atma"* is rare and embeds near nothing in particular.
Cosine alone is a popularity contest the corpus median wins.

So the final ranking is explicitly two-term, after Generative Agents but with
diagnosticity in place of poignancy:

```
score = w_r · relevance + w_d · diagnosticity        (both already in [0,1])
```

✎ **The weights are free parameters, not 1.** v2 wrote `relevance +
diagnosticity`, which is α = 1 — exactly what `res-theory.md` warns against,
since Generative Agents never tuned those weights and the paper's own text flags
them as untuned. `w_r` and `w_d` are swept on the tuning split (§7), starting at
1/1 as a baseline rather than as an answer.

✎ **No min-max normalisation.** v2 normalised both terms across the candidate
set. Wrong scope and unnecessary: the reranker's score is already bounded, and
diagnosticity is a bounded combination by construction. Per-candidate-set
normalisation would also make the scores incomparable between queries, which
breaks the raw-score abstention floor in §5.

- **relevance** = the reranker's score (§4).
- **diagnosticity** = a cheap, measured, *transparent* combination:
  `emphasis` (profanity/emphasis markers: 59% in rejections, 67% in granular
  nudges, vs 33% in approvals), `len_band` (80–800 chars carries the judgment;
  <40 is "ok"/"go", >2000 is a paste), and rule language ("asla", "never",
  "always"). All recomputed on read, all inspectable.

✎ **No recency term.** Deliberate and against instinct: the corpus spans
2026-05-08 → 2026-08-11, so there is no long-horizon drift to model, and the
reaction-class mix is measurably frozen across halves (D 32.6→32.5, Q 23.8→26.3,
S 7.3→7.6, R 5.7→6.0) while project mix moves a lot. Style is stable; topic
drifts. The visible declines (`"planla"` −6.2z, `"lets go"` −5.7z) are best
explained by those preferences migrating into `CLAUDE.md` and skills — he stopped
*having* to say them. Decaying the older half would delete the explicit
statements of exactly the preferences that later became implicit.

✎ **Re-evaluation trigger, so this is a decision and not an assumption:** revisit
when the corpus spans >12 months, or when re-running the §7 eval shows accuracy
on recent held-out sessions falling below accuracy on older ones. ✎✎ Class-mix
drift was v2's trigger and is the wrong instrument: the mix stayed frozen while
project mix moved a lot, so it is blind to precisely the drift that would matter
— the user changing his mind about *how* to judge a given kind of work while
still producing the same distribution of reaction types. Timestamps are stored
via `src_message_fk` for exactly this check.

### 4. Retrieval (`wend recall`)

1. **Vector** — cosine over case vectors, top 40. Past ~50 the cross-encoder cost
   is linear and recall gain is flat.
2. **Rerank** — Cohere `rerank-v4.0-fast` over candidate **situations**, not
   reactions. A reranker scores "does this document answer this query"; a
   reaction does not answer a situation, but a past situation genuinely is or is
   not similar to the current one. Endpoint verified:
   `POST {endpoint}providers/cohere/v2/rerank`.
3. **Reaction dedup** ✎ — the duplication worth killing is eight retrieved cases
   that all say "hadi go", which is redundancy in the *reaction*, not the
   situation. v2 proposed embedding the reaction side for MMR; that is not
   expressible in the schema, because `chunk_vectors.chunk_fk` is the PRIMARY KEY
   (`schema.rs:37`) — one vector per chunk, with nowhere to put a second.

   Rather than add a vector slot, dedupe on **normalised exact equality** of the
   reaction (casefold, collapse whitespace, strip punctuation and repeated
   emphasis). res-data measured 391 exact normalised duplicates in the corpus, so
   this is where the redundancy actually is. Costs nothing, adds no column,
   embeds nothing extra.

   ✎ **Fuzzy similarity is deliberately rejected here.** A trigram-Jaccard
   threshold was proposed and withdrawn: *"bunu yap"* vs *"bunu yapma"* scores
   0.75, and *"this is acceptable"* vs *"this is not acceptable"* scores 0.70, so
   a >0.6 rule would discard the exact polarity contrast that §4.4 exists to
   preserve. Any fuzzy variant needs a negation-sensitive guard and must be
   validated before it ships.

   Semantic near-duplicates ("tamam devam" vs "hadi go") survive both. That is a
   **measured** follow-up: if §7 shows them crowding the final set, add reaction
   vectors then — as a second row per case, not a second column.
4. **Polarity balance** — the final set must contain at least one approval-class
   and one rejection-class case *when both clear the floor*. This is a sycophancy
   guard: we retrieve the user's past agreement and condition on it, so unbalanced
   retrieval makes the model approve what this user would have hated.

Output 5–8 cases. More is measured as not helping, and at many-shot scale
similarity-based selection converges toward random, which would neutralise the
retriever.

**Ordering is a design decision.** Models over-weight what sits near the end of
the prompt, and permuting the same exemplars can swing results between near-SOTA
and chance. Emit in ascending diagnosticity so the most diagnostic case is last,
deterministically, or evals will not reproduce.

### 5. Abstention ✎ — decoupled from polarity

v1 said abstain when "the retrieved set disagrees with itself" while *mandating*
mixed polarity — so every successfully balanced result would abstain. Reviewers
caught the contradiction; it is removed.

Abstention is now **one condition**: the top reranked candidate's **raw**
relevance score is below a floor. ✎ Raw, not the min-max-normalised score from
§3 — normalisation puts the top candidate at exactly 1.0 by construction, so
thresholding it could never fire. ✎ The floor is not guessed — it is swept empirically. ✎✎ **It is swept on a
separate tuning split, never on the sample that reports accuracy.** v2 fitted the
floor on the same fixed-100 hand-graded set it then reported results from, which
is fitting a hyperparameter on the test set and would have reported a number
that cannot be reproduced. Sessions are partitioned three ways: **tune** (sweep
`w_r`, `w_d` and both floors), **report** (the fixed 100, hand-graded, touched
once), and the rest unused. The value chosen maximises F1 between "answered and
right" and "abstained on a pivot" on **tune** only. 24.2% of reactions are pivots that no
situation representation recovers; the honest behaviour there is to say so
rather than return a forced nearest neighbour.

### 6. Azure configuration ✎

Rerank needs a **second deployment**, and v1 had nowhere to put it.

- New **optional** `WEND_AZURE_RERANK_DEPLOYMENT`, sharing the existing endpoint
  and key. It stays outside `Config::from_env`'s all-three-or-none rule, so
  existing embedding-only setups keep working untouched.
- ✎ It must **not** appear in `model_id()` (`azure.rs:109`). That string is the
  vector storage key; folding the rerank deployment into it would invalidate
  every chunk vector in the index the moment the user changed rerankers.
- ✎ **Every string sent to the reranker goes through `redact()` first.** The
  rerank request carries the current situation *and* 40 stored situations, each
  containing `prev_user` prose. Embeddings already redact
  (`azure.rs:166`) — the corpus is known to contain live AWS keys, GitHub and npm
  tokens, bearer tokens and DB URIs. Omitting redaction here reopens exactly the
  hole that was closed for embeddings.

✎ **Rerank is optional, not mandatory.** A `--features semantic` build has no
Cohere client. `recall` there runs vector → dedup → polarity and says which mode
it used. ✎ And it is the **default** path for anyone without an Azure rerank deployment,
so it must be fully specified rather than described as a fallback: `relevance` is
the raw cosine similarity, the same `w_r`/`w_d` scoring applies, and it carries
**its own separately swept floor**. Cosine and cross-encoder relevance are
different quantities on different scales; reusing one threshold for both would
silently mis-set abstention on whichever path was not tuned. Both paths are
swept on the tuning split and both floors are reported. Only the Cohere client is Azure-gated; the case library and
retrieval work on either backend.

### 7. Evaluation ✎ — v1's could not fail

v1 graded "does the retrieved set contain a reaction of the same class", using
the same 56.7% classifier for both the library and the grading. A pivot labelled
D matching an unrelated retrieved D scored as correct.

Replacement:
- **Leave-one-session-out.** Hold out whole sessions, never individual pairs, so
  neighbouring turns from the same conversation cannot leak.
- **Report stage-1 recall@40 separately** — it is the ceiling everything after it
  inherits. ✎ It needs a defined relevant set, which leave-one-session-out alone
  does not provide. Definition used: for a held-out case, a candidate counts as
  relevant if a human judged it "a situation where the same reaction would have
  been reasonable" during the fixed-100 grading pass. So recall@40 is computed
  over that same hand-graded sample, not over the whole corpus — a smaller,
  honest number rather than a large, undefined one.
- **Grade by hand on a fixed 100-case sample**, not by the classifier. The
  classifier may propose; it may not score.
- **Condition on responsive cases.** The 73.3% figure bounds only those; mixing
  pivots in lets an unrelated match count as a success.
- **Calibration, not just accuracy.** If rejection is ~14% of reactions and the
  retrieved sets surface rejection at 3%, the system is broken whatever its
  per-item accuracy. Report predicted-vs-actual class base rates.
- **Normalise against the user's own self-consistency**, not against 100%. He
  will not answer identically twice; chasing 100% fits noise.

### 8. CLI surface

```
wend recall "<situation text>" [--limit 8] [--json]
wend index --embed        # also builds and embeds the case library
```

`recall` prints each case: situation, the verbatim reaction, computed class,
diagnosticity, date, session id. `--json` for the skill.

## Explicitly not in scope

- Any extraction pipeline that *replaces* raw storage — measured regression.
- Knowledge graphs: they win on multi-hop entity queries; ours is exemplar
  retrieval, and a graph DB ends the single-binary story.
- `valid_until` / preference supersession — real, but needs a distilled-preference
  layer that does not exist yet.
- Inverse-HyDE — promising, must be measured before it is built.
- The `corrected` outcome slot — CBR's fourth R and genuinely interesting, but it
  has no consumer until there is a diagnosticity learner. Deferred, not rejected.
- The skill that consumes `recall`, and any agent that answers *as* the user.
- `compile_query`'s AND-ing, still open from the previous change.

## Measured outcome ✎ — the verdict half does not work

Built and evaluated. The retrieval, storage, cost and abstention all landed where
the plan said. The *goal* did not: over 477 held-out probes (120 sessions held
out), retrieval predicts the user's stance **no better than random and trending
worse** — majority vote 35.8% against a 43.4% random baseline. A frontier model
handed the same situations could not beat always-guessing-the-majority-class
(34.0% and 29.5% against 56.0%).

The cause is structural, not a defect: the same situation precedes approval or
rejection depending on whether the work was good, and work quality is not in the
indexed text. No classifier, reward model, DPO run or LoRA fixes a label that
isn't a function of the features.

What does survive is the criteria signal — retrieved reactions share 1.66× more
vocabulary with the real reaction than random ones. So `recall` is a standards
lookup, not a predictor, and the output says so. Full numbers, method and
caveats in `evals/RESULTS.md`.

Two things this changes for anyone extending it: the diagnosticity term and
polarity balance deliberately skew the retrieved mix toward rejection (36% vs a
true 17%), which is why it teaches well and predicts badly; and if verdict
prediction is ever wanted, the missing feature is artifact quality, recoverable
post-hoc from the transcript (did tests pass, was it reverted, how many
correction turns followed).

## Safety constraint (non-negotiable)

`recall` output is **evidence, not authority**. The user's standing rules include
"never commit/push/release without my explicit approval". A system that predicts
their approval and acts on it launders exactly that rule. `recall` is read-only
and advisory: it reports what they said before, with provenance, and never
converts a retrieved approval into consent for an irreversible action.

## Verification

1. `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` across all four
   feature combinations (default / semantic / azure / semantic,azure).
2. Unit: turn assembler (tool_result rows are not boundaries; sidechain rows
   skipped; tool names with counts; char-boundary slicing on Turkish); v5
   migration is additive, single-batch and idempotent; `kind` filter keeps cases
   out of ordinary semantic search **and** `sessions_without_chunks` still finds
   prose-less sessions that already have cases; dedup collapses exact-normalised
   duplicate reactions and leaves polarity opposites intact; polarity balance holds when both classes clear the
   floor; abstention fires below floor; ordering is deterministic; redaction runs
   on every rerank input.
3. Live: build and embed all ~6,982 cases; confirm cost lands near **$0.375**
   (situations only — reactions are not embedded).
4. Retrieval quality per §7, with the abstention floor swept rather than guessed.
5. Confirm ordinary `wend search --semantic` results are unchanged by the
   presence of cases in the `chunks` table.
