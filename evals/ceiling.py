"""Is the user's stance predictable from the situation at all?

This bounds every training question that comes after it. If a strong LLM cannot
beat the majority-class baseline at predicting whether this user will approve,
reject or stay neutral, then the label is not a function of the input — and no
smaller model, classifier or fine-tune can do better, because the information
simply is not there. Training would be fitting noise.

Three conditions, same held-out probes:
  1. majority class      — always guess the most common stance
  2. LLM, situation only — can a strong model read the situation and call it?
  3. LLM + precedents    — does giving it the user's own past reactions help?

Condition 3 is the one that matters for the product: it is exactly what an agent
would do with `wend recall` output.
"""

import json
import os
import random
import shutil
import sqlite3
import subprocess
import sys
import urllib.error
import urllib.request

SCRATCH = os.environ.get("WEND_EVAL_DIR", os.path.expanduser("~/.cache/wend-evals"))
EVAL = f"{SCRATCH}/eval-index.db"
WEND = os.environ.get("WEND_BIN", "wend")
EP = open(f"{SCRATCH}/ep.txt").read().strip()
KEY = open(f"{SCRATCH}/key.txt").read().strip()
MODEL = "DeepSeek-V4-Pro"
N = 50

sys.path.insert(0, SCRATCH)
from eval_recall import stance  # same port of the Rust stance_of  # noqa: E402


def ask(prompt):
    body = {
        "model": MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 8,
        "temperature": 0,
    }
    req = urllib.request.Request(
        f"{EP}openai/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"api-key": KEY, "Content-Type": "application/json"},
    )
    for attempt in range(5):
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                return json.load(r)["choices"][0]["message"]["content"].strip().lower()
        except urllib.error.HTTPError as e:
            if e.code != 429 or attempt == 4:
                raise
            import time

            time.sleep(2**attempt)
    return ""


def parse(reply):
    for s in ("reject", "approve", "neutral"):
        if s in reply:
            return s
    return "neutral"


BRIEF = (
    "A developer is working with an AI coding agent. Below is what the agent had just "
    "said or done. Predict how this specific developer reacted next.\n\n"
    "Answer with exactly one word: approve, reject, or neutral.\n"
    "approve = he agreed / told it to proceed. reject = he pushed back, corrected, or "
    "challenged the claim. neutral = he asked something else or changed direction.\n\n"
    "SITUATION:\n{situation}\n\nOne word:"
)

WITH_CASES = (
    "A developer is working with an AI coding agent. Below is what the agent had just "
    "said or done, followed by how this same developer really reacted in similar past "
    "situations — his own words.\n\n"
    "Answer with exactly one word: approve, reject, or neutral.\n"
    "approve = he agreed / told it to proceed. reject = he pushed back, corrected, or "
    "challenged the claim. neutral = he asked something else or changed direction.\n\n"
    "HIS PAST REACTIONS IN SIMILAR SPOTS:\n{cases}\n\n"
    "CURRENT SITUATION:\n{situation}\n\nOne word:"
)


def main():
    random.seed(11)
    conn = sqlite3.connect(f"file:{EVAL}?mode=ro", uri=True)
    full = sqlite3.connect(f"file:{SCRATCH}/test-index.db?mode=ro", uri=True)

    # Probe ONLY with sessions whose cases were removed from the eval index.
    # Drawing from the full index would hand the retriever the answer.
    still_in = {r[0] for r in conn.execute(
        "SELECT DISTINCT session_fk FROM chunks WHERE kind='case'")}
    rows = full.execute(
        "SELECT session_fk, text, COALESCE(payload,'') FROM chunks WHERE kind='case'"
    ).fetchall()
    probes = [(s, r) for fk, s, r in rows if fk not in still_in and len(r) > 15]
    assert probes, "no held-out probes — run eval_recall.py first to build the eval index"
    random.shuffle(probes)
    probes = probes[:N]

    env = {
        **os.environ,
        "WEND_DB": EVAL,
        "WEND_AZURE_ENDPOINT": EP,
        "WEND_AZURE_KEY": KEY,
        "WEND_AZURE_DEPLOYMENT": "wend-embed-oai",
        "WEND_AZURE_RERANK_DEPLOYMENT": "wend-rerank",
    }

    truths, solo, aided = [], [], []
    for situation, reaction in probes:
        truth = stance(reaction)
        truths.append(truth)
        solo.append(parse(ask(BRIEF.format(situation=situation[-1500:]))))

        out = subprocess.run(
            [WEND, "recall", situation, "--json", "--limit", "5"],
            env=env, capture_output=True, text=True, timeout=240,
        )
        d = json.loads(out.stdout or "{}") if out.returncode == 0 else {}
        ps = d.get("precedents", [])
        if ps:
            cases = "\n".join(f"- ({p['stance']}) {p['reaction'][:200]}" for p in ps)
            aided.append(parse(ask(WITH_CASES.format(cases=cases, situation=situation[-1500:]))))
        else:
            aided.append(None)

    n = len(truths)
    major = max(set(truths), key=truths.count)
    base = truths.count(major) / n
    acc_solo = sum(a == b for a, b in zip(solo, truths)) / n
    paired = [(a, b) for a, b in zip(aided, truths) if a is not None]
    acc_aided = sum(a == b for a, b in paired) / max(len(paired), 1)

    print(f"probes {n}, majority class '{major}'\n")
    print(f"{'condition':32}{'accuracy':>10}")
    print("-" * 42)
    print(f"{'always guess majority':32}{base:>9.1%}")
    print(f"{'LLM, situation only':32}{acc_solo:>9.1%}")
    print(f"{'LLM + his own precedents':32}{acc_aided:>9.1%}  (n={len(paired)})")
    print(f"\npredicted mix solo : { {s: solo.count(s) for s in set(solo)} }")
    print(f"actual mix         : { {s: truths.count(s) for s in set(truths)} }")


if __name__ == "__main__":
    main()
