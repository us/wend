"""Is there a pattern between what the agent said and how the user replied?

The earlier eval concluded "not predictable", but it graded with a keyword regex
that agrees with hand labels only ~57% of the time. That is a weak instrument,
and a null result from a weak instrument is not evidence of a null effect — it
may just have destroyed the signal it was looking for.

So label both sides properly instead. An LLM classifies the agent's move (does it
claim completion, ask permission, present options, report an error, ask a
question) and, separately, the user's reply (approve, demand proof, correct,
redirect, new task). Neither classifier sees the other side, so the labels cannot
leak into each other.

Then: does the agent's move predict the user's reply above the base rate? That is
the pattern claim, tested directly.
"""

import collections
import json
import os
import random
import sqlite3
import urllib.error
import urllib.request

SCRATCH = os.environ.get("WEND_EVAL_DIR", os.path.expanduser("~/.cache/wend-evals"))
FULL = f"{SCRATCH}/test-index.db"
EP = open(f"{SCRATCH}/ep.txt").read().strip()
KEY = open(f"{SCRATCH}/key.txt").read().strip()
MODEL = "DeepSeek-V4-Pro"
N = 200

MOVES = ["claims_done", "asks_permission", "presents_options", "reports_problem", "asks_question"]
REPLIES = ["approve", "demand_proof", "correct", "redirect", "new_task"]

MOVE_PROMPT = """Classify what an AI coding agent's message is DOING. Ignore the topic.

claims_done      - reports work finished, tests passing, verified, deployed
asks_permission  - proposes an action and waits for a go-ahead
presents_options - lays out alternatives and asks which one
reports_problem  - reports an error, failure, or something blocked
asks_question    - asks for information or clarification

Reply with exactly one label from that list, nothing else.

MESSAGE:
{text}

Label:"""

REPLY_PROMPT = """Classify what a developer's reply to their AI coding agent is DOING. Ignore the topic.

approve      - agrees, tells it to proceed, says it looks good
demand_proof - doubts a claim, asks whether it was really tested/verified
correct      - says it is wrong, was done badly, or must be redone
redirect     - accepts but changes the direction or adds a constraint
new_task     - moves on to something unrelated

Reply with exactly one label from that list, nothing else.

REPLY:
{text}

Label:"""


def ask(prompt):
    body = {"model": MODEL, "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 8, "temperature": 0}
    req = urllib.request.Request(
        f"{EP}openai/v1/chat/completions", data=json.dumps(body).encode(),
        headers={"api-key": KEY, "Content-Type": "application/json"})
    for attempt in range(5):
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                return json.load(r)["choices"][0]["message"]["content"].strip().lower()
        except urllib.error.HTTPError as e:
            if e.code != 429 or attempt == 4:
                raise
            import time
            time.sleep(2 ** attempt)
    return ""


def pick(reply, options):
    for o in options:
        if o in reply:
            return o
    return None


def main():
    random.seed(31)
    conn = sqlite3.connect(f"file:{FULL}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT text, COALESCE(payload,'') FROM chunks WHERE kind='case'").fetchall()
    pairs = [(s, r) for s, r in rows if len(s) > 200 and 30 < len(r) < 500]
    random.shuffle(pairs)

    data = []
    for situation, reaction in pairs:
        if len(data) >= N:
            break
        move = pick(ask(MOVE_PROMPT.format(text=situation[-1200:])), MOVES)
        rep = pick(ask(REPLY_PROMPT.format(text=reaction[:500])), REPLIES)
        if move and rep:
            data.append((move, rep))

    n = len(data)
    reply_counts = collections.Counter(r for _, r in data)
    base = reply_counts.most_common(1)[0][1] / n

    # Best achievable by always guessing the most common reply for each move.
    by_move = collections.defaultdict(collections.Counter)
    for m, r in data:
        by_move[m][r] += 1
    conditional = sum(c.most_common(1)[0][1] for c in by_move.values()) / n

    print(f"pairs labelled: {n}\n")
    # Full contingency: argmax accuracy is blind when one reply class dominates.
    # A real pattern can live in the distribution while the top label never moves.
    hdr = "".join(f"{r[:9]:>11}" for r in REPLIES)
    print(f"{'agent move':18}{'n':>5}{hdr}")
    print("-" * (23 + 11 * len(REPLIES)))
    for m in MOVES:
        c = by_move.get(m)
        if not c:
            continue
        t = sum(c.values())
        cells = "".join(f"{c.get(r,0)/t:>10.0%} " for r in REPLIES)
        print(f"{m:18}{t:>5}{cells}")
    t = sum(reply_counts.values())
    cells = "".join(f"{reply_counts.get(r,0)/t:>10.0%} " for r in REPLIES)
    print(f"{'ALL (base rate)':18}{t:>5}{cells}")

    # Chi-square + Cramer's V: is the move->reply association above chance?
    import math
    chi = 0.0
    for m in by_move:
        rm = sum(by_move[m].values())
        for r in REPLIES:
            exp = rm * reply_counts.get(r, 0) / n
            if exp > 0:
                chi += (by_move[m].get(r, 0) - exp) ** 2 / exp
    dof = (len(by_move) - 1) * (len(REPLIES) - 1)
    v = math.sqrt(chi / (n * min(len(by_move) - 1, len(REPLIES) - 1)))
    print(f"\nchi-square {chi:.1f} on {dof} dof, Cramer's V {v:.3f}"
          f"  (V<0.1 negligible, 0.1-0.3 weak, >0.3 moderate)")

    print(f"\nalways guess the most common reply overall : {base:.1%}")
    print(f"guess the best reply for each agent move   : {conditional:.1%}")
    print(f"lift from knowing what the agent did       : {conditional - base:+.1%}")
    print(f"\noverall reply mix: {dict(reply_counts)}")


if __name__ == "__main__":
    main()
