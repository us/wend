"""Leave-one-session-out evaluation of `wend recall`, with a random baseline.

The question: given a situation the retriever has never seen, do the precedents
it returns actually predict how the user reacted?

Whole sessions are held out, never individual cases, so a neighbouring turn from
the same conversation cannot leak. Grading is by STANCE agreement — did the
retrieved set contain a precedent whose stance matches what the user actually
said — which is coarse but computable without hand labels.

The point of the random baseline is that this eval can FAIL. If retrieval
carried no signal, stance agreement would equal what you get from drawing the
same number of cases at random. Beating that baseline by a wide margin is the
only result that means anything.
"""

import json
import os
import random
import shutil
import sqlite3
import subprocess
import sys

SCRATCH = os.environ.get("WEND_EVAL_DIR", os.path.expanduser("~/.cache/wend-evals"))
SRC = f"{SCRATCH}/test-index.db"
EVAL = f"{SCRATCH}/eval-index.db"
WEND = os.environ.get("WEND_BIN", "wend")
HELD_OUT_SESSIONS = 120
SAMPLE = 500
LIMIT = 5

# Verbatim port of stance_of() in crates/wend-core/src/embed/cases.rs. Kept in
# lockstep by hand; if the Rust lists change this must change with them.
REJECT = ["hayir", "hayır", "olmamis", "olmamış", "yanlis", "yanlış", "neden", "niye", "asla",
          "gerek yok", "bosver", "boşver", "duzelt", "düzelt", "emin misin", "test ettin",
          "anlamadin", "anlamadın", "yapmamissin", "yapmamışsın", "sacma", "saçma", "no ",
          "don't", "dont ", "wrong", "revert"]
APPROVE = ["tamam", "guzel", "güzel", "hadi", "lets go", "let's go", "go ", "olur", "evet", "ok ",
           "harika", "devam et", "yap ", "merge"]


def stance(reaction):
    r = reaction.lower()
    if any(n in r for n in REJECT):
        return "reject"
    if any(n in r for n in APPROVE):
        return "approve"
    return "neutral"


def main():
    random.seed(11)
    shutil.copyfile(SRC, EVAL)
    conn = sqlite3.connect(EVAL)

    sessions = [r[0] for r in conn.execute(
        "SELECT session_fk FROM chunks WHERE kind='case' GROUP BY session_fk HAVING count(*) >= 3"
    ).fetchall()]
    random.shuffle(sessions)
    held = sessions[:HELD_OUT_SESSIONS]
    marks = ",".join("?" * len(held))

    cases = conn.execute(
        f"SELECT text, COALESCE(payload,'') FROM chunks "
        f"WHERE kind='case' AND session_fk IN ({marks})", held
    ).fetchall()
    # Remove the held-out sessions' cases entirely, vectors included.
    ids = [r[0] for r in conn.execute(
        f"SELECT id FROM chunks WHERE kind='case' AND session_fk IN ({marks})", held).fetchall()]
    conn.executemany("DELETE FROM chunk_vectors WHERE chunk_fk=?", [(i,) for i in ids])
    conn.executemany("DELETE FROM chunks WHERE id=?", [(i,) for i in ids])
    conn.commit()

    library = [r[0] for r in conn.execute(
        "SELECT COALESCE(payload,'') FROM chunks WHERE kind='case'").fetchall()]
    remaining = conn.execute(
        "SELECT count(*) FROM chunks WHERE kind='case'").fetchone()[0]
    conn.close()

    random.shuffle(cases)
    probes = [(s, r) for s, r in cases if len(r) > 15][:SAMPLE]
    print(f"held out {len(held)} sessions ({len(ids)} cases); library keeps {remaining}", file=sys.stderr)
    print(f"probing {len(probes)} situations\n", file=sys.stderr)

    env = {**os.environ, "WEND_DB": EVAL,
           "WEND_AZURE_ENDPOINT": open(f"{SCRATCH}/ep.txt").read().strip(),
           "WEND_AZURE_KEY": open(f"{SCRATCH}/key.txt").read().strip(),
           "WEND_AZURE_DEPLOYMENT": "wend-embed-oai",
           "WEND_AZURE_RERANK_DEPLOYMENT": "wend-rerank"}

    def majority(stances):
        return max(set(stances), key=stances.count) if stances else None

    # "Any of 5 matches" is nearly unfailable with 3 classes — measured at 89.1%
    # retrieval against an 83.6% random baseline, i.e. it tells you almost
    # nothing. These three are the ones that can actually separate a working
    # retriever from a broken one.
    score = {k: 0 for k in ("any", "majority", "top1")}
    base = {k: 0 for k in ("any", "majority", "top1")}
    disagree = {k: [0, 0] for k in ("any", "majority", "top1")}  # [ret-only, rnd-only]
    answered = abstained = 0
    actual_mix, predicted_mix = {}, {}

    for situation, reaction in probes:
        truth = stance(reaction)
        actual_mix[truth] = actual_mix.get(truth, 0) + 1
        out = subprocess.run(
            [WEND, "recall", situation, "--json", "--limit", str(LIMIT)],
            env=env, capture_output=True, text=True, timeout=240)
        if out.returncode != 0:
            continue
        d = json.loads(out.stdout or "{}")
        if d.get("abstained", True):
            abstained += 1
            continue
        answered += 1
        ps = d["precedents"]
        got = [p["stance"] for p in ps]
        for g in got:
            predicted_mix[g] = predicted_mix.get(g, 0) + 1
        # Strongest single precedent, by relevance rather than print order.
        best = max(ps, key=lambda p: p["relevance"])["stance"]

        drawn = [stance(x) for x in random.sample(library, len(got))]
        outcomes = {
            "any": (truth in got, truth in drawn),
            "majority": (majority(got) == truth, majority(drawn) == truth),
            "top1": (best == truth, drawn[0] == truth),
        }
        for k, (r_ok, b_ok) in outcomes.items():
            score[k] += r_ok
            base[k] += b_ok
            if r_ok and not b_ok: disagree[k][0] += 1
            elif b_ok and not r_ok: disagree[k][1] += 1

    n = max(answered, 1)
    import math
    def ci(k, n):
        """Wilson 95% interval — normal approximation is wrong at these counts."""
        if n == 0: return (0.0, 0.0)
        p, z = k / n, 1.96
        d = 1 + z*z/n
        c = (p + z*z/(2*n)) / d
        h = z*math.sqrt(p*(1-p)/n + z*z/(4*n*n)) / d
        return (max(0.0, c-h), min(1.0, c+h))
    def mcnemar(b, c):
        """Paired test: b = retrieval right/random wrong, c = the reverse."""
        if b + c == 0: return 1.0
        chi = (abs(b - c) - 1) ** 2 / (b + c)
        return math.exp(-chi / 2)  # 1-dof chi-square upper-tail approximation

    print(f"{'metric':24}{'retrieval':>11}{'random':>9}{'lift':>8}")
    print("-" * 52)
    for k, label in [("any", "any of 5 matches"), ("majority", "majority vote"),
                     ("top1", "strongest precedent")]:
        r, b = score[k] / n, base[k] / n
        lo, hi = ci(score[k], n)
        p = mcnemar(disagree[k][0], disagree[k][1])
        print(f"{label:24}{r:>10.1%}{b:>9.1%}{r - b:>+8.1%}   [{lo:.1%}-{hi:.1%}] p~{p:.3f}")
    print(f"\nanswered {answered}, abstained {abstained} of {len(probes)}")
    print(f"actual stance mix   : {actual_mix}")
    print(f"retrieved stance mix: {predicted_mix}")


if __name__ == "__main__":
    main()
