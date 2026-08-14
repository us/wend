"""Can retrieved precedents make a model say what this user would have said?

The earlier evals classified reactions into coarse buckets and counted the
buckets. That was the wrong instrument: in any individual case it is obvious
what the user wanted, and bucketing throws exactly that away.

So measure content directly. For each held-out situation, generate a reply three
ways and see which lands closest to what the user really wrote, in embedding
space:

  generic   - a model answering as a generic developer, no history at all
  precedent - the same model, given what `wend recall` returns for this situation
  random    - one of the user's real reactions drawn at random

`random` is the floor that matters: it is what you score by knowing this user's
general style but nothing about the moment. Beating `generic` only shows the
history helps; beating `random` shows the *retrieval* helps, not just the persona.
"""

import json
import math
import os
import random
import sqlite3
import subprocess
import urllib.error
import urllib.request

SCRATCH = os.environ.get("WEND_EVAL_DIR", os.path.expanduser("~/.cache/wend-evals"))
EVAL = f"{SCRATCH}/eval-index.db"
FULL = f"{SCRATCH}/test-index.db"
WEND = os.environ.get("WEND_BIN", "wend")
EP = open(f"{SCRATCH}/ep.txt").read().strip()
KEY = open(f"{SCRATCH}/key.txt").read().strip()
WRITER = "DeepSeek-V4-Pro"
N = 60

GENERIC = """An AI coding agent just sent this message to the developer it works for.
Write the developer's realistic next message. Output only the message.

AGENT:
{situation}

Developer's reply:"""

WITH_CASES = """An AI coding agent just sent this message to the developer it works for.
Here is how that same developer really replied in similar moments before.

Write his next message: what he would actually want here, in his register.
Output only the message.

HIS REAL REPLIES IN SIMILAR MOMENTS:
{cases}

AGENT:
{situation}

His reply:"""


def post(path, body, timeout=180):
    req = urllib.request.Request(
        f"{EP}{path}", data=json.dumps(body).encode(),
        headers={"api-key": KEY, "Content-Type": "application/json"})
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code != 429 or attempt == 5:
                raise
            import time
            time.sleep(2 ** attempt)
    return {}


def write(prompt):
    d = post("openai/v1/chat/completions",
             {"model": WRITER, "messages": [{"role": "user", "content": prompt}],
              "max_tokens": 220, "temperature": 0.4})
    return d["choices"][0]["message"]["content"].strip() if d else ""


def embed(texts):
    out = []
    for i in range(0, len(texts), 32):
        d = post("openai/v1/embeddings",
                 {"model": "wend-embed-oai", "input": texts[i:i + 32], "dimensions": 1024})
        out += [x["embedding"] for x in sorted(d["data"], key=lambda x: x["index"])]
    return out


def cos(a, b):
    return sum(x * y for x, y in zip(a, b))


def main():
    random.seed(41)
    ev = sqlite3.connect(f"file:{EVAL}?mode=ro", uri=True)
    full = sqlite3.connect(f"file:{FULL}?mode=ro", uri=True)
    still_in = {r[0] for r in ev.execute(
        "SELECT DISTINCT session_fk FROM chunks WHERE kind='case'")}
    rows = full.execute(
        "SELECT session_fk, text, COALESCE(payload,'') FROM chunks WHERE kind='case'").fetchall()
    probes = [(s, r) for fk, s, r in rows if fk not in still_in and 40 < len(r) < 400]
    assert probes, "no held-out probes — run eval_recall.py first"
    random.shuffle(probes)
    probes = probes[:N]
    library = [r[0] for r in ev.execute(
        "SELECT COALESCE(payload,'') FROM chunks WHERE kind='case'") if len(r[0]) > 40]

    env = {**os.environ, "WEND_DB": EVAL, "WEND_AZURE_ENDPOINT": EP, "WEND_AZURE_KEY": KEY,
           "WEND_AZURE_DEPLOYMENT": "wend-embed-oai",
           "WEND_AZURE_RERANK_DEPLOYMENT": "wend-rerank"}

    actual, gen_generic, gen_cases, rnd = [], [], [], []
    for situation, real in probes:
        out = subprocess.run([WEND, "recall", situation, "--json", "--limit", "6"],
                             env=env, capture_output=True, text=True, timeout=240)
        d = json.loads(out.stdout or "{}") if out.returncode == 0 else {}
        ps = d.get("precedents", [])
        if not ps:
            continue
        cases = "\n".join(f"- {p['reaction'][:220]}" for p in ps)
        g1 = write(GENERIC.format(situation=situation[-1200:]))
        g2 = write(WITH_CASES.format(cases=cases, situation=situation[-1200:]))
        if not g1 or not g2:
            continue
        actual.append(real)
        gen_generic.append(g1)
        gen_cases.append(g2)
        rnd.append(random.choice(library))

    n = len(actual)
    va, vg, vc, vr = (embed(actual), embed(gen_generic), embed(gen_cases), embed(rnd))
    s_generic = [cos(a, b) for a, b in zip(va, vg)]
    s_cases = [cos(a, b) for a, b in zip(va, vc)]
    s_rnd = [cos(a, b) for a, b in zip(va, vr)]

    def mean(x):
        return sum(x) / len(x)

    def stderr(x):
        m = mean(x)
        return math.sqrt(sum((v - m) ** 2 for v in x) / (len(x) - 1) / len(x))

    print(f"probes {n}\n")
    print(f"{'reply written as':34}{'similarity to what he said':>28}")
    print("-" * 62)
    for label, s in [("a generic developer", s_generic),
                     ("one of his real replies, at random", s_rnd),
                     ("model + his retrieved precedents", s_cases)]:
        print(f"{label:34}{mean(s):>20.3f} ±{stderr(s):.3f}")

    wins = sum(c > g for c, g in zip(s_cases, s_generic))
    wins_r = sum(c > r for c, r in zip(s_cases, s_rnd))
    print(f"\nprecedents beat generic on {wins}/{n} probes ({wins / n:.0%})")
    print(f"precedents beat a random real reply on {wins_r}/{n} ({wins_r / n:.0%})")


if __name__ == "__main__":
    main()
