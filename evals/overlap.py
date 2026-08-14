"""Does retrieval surface the right KIND of demand, even when it misses the verdict?

Stance turned out to be unpredictable. This asks a weaker but more useful
question: do the retrieved reactions share vocabulary with what the user actually
said — the criteria he raises (test, proof, cost, competitor, show me) — more
than randomly drawn reactions do?
"""
import json
import os, os, random, re, sqlite3, subprocess, sys
S = SCRATCH
WEND = os.environ.get("WEND_BIN", "wend")
STOP = set("bu su o ve ile de da mi mu ne icin bir bi ama yani gibi daha cok en var yok "
           "the a an and or of to is it that this for on in be do you i we".split())
def toks(s):
    return {w for w in re.findall(r"[a-zçğıöşü]{4,}", s.lower()) if w not in STOP}
def jac(a, b):
    return len(a & b) / len(a | b) if (a | b) else 0.0
random.seed(11)
ev, full = sqlite3.connect(f"file:{S}/eval-index.db?mode=ro", uri=True), sqlite3.connect(f"file:{S}/test-index.db?mode=ro", uri=True)
inn = {r[0] for r in ev.execute("SELECT DISTINCT session_fk FROM chunks WHERE kind='case'")}
rows = full.execute("SELECT session_fk,text,COALESCE(payload,'') FROM chunks WHERE kind='case'").fetchall()
probes = [(s, r) for fk, s, r in rows if fk not in inn and len(r) > 40]
random.shuffle(probes); probes = probes[:60]
lib = [r[0] for r in ev.execute("SELECT COALESCE(payload,'') FROM chunks WHERE kind='case'")]
env = {**os.environ, "WEND_DB": f"{S}/eval-index.db", "WEND_AZURE_ENDPOINT": open(f"{S}/ep.txt").read().strip(),
       "WEND_AZURE_KEY": open(f"{S}/key.txt").read().strip(), "WEND_AZURE_DEPLOYMENT": "wend-embed-oai",
       "WEND_AZURE_RERANK_DEPLOYMENT": "wend-rerank"}
got, base, n = [], [], 0
for sit, react in probes:
    t = toks(react)
    if not t: continue
    o = subprocess.run([WEND, "recall", sit, "--json", "--limit", "5"], env=env, capture_output=True, text=True, timeout=240)
    d = json.loads(o.stdout or "{}") if o.returncode == 0 else {}
    ps = d.get("precedents", [])
    if not ps: continue
    n += 1
    got.append(max(jac(t, toks(p["reaction"])) for p in ps))
    base.append(max(jac(t, toks(x)) for x in random.sample(lib, len(ps))))
print(f"probes {n}")
print(f"  retrieval best-of-5 overlap : {sum(got)/n:.4f}")
print(f"  random    best-of-5 overlap : {sum(base)/n:.4f}")
print(f"  lift                        : {(sum(got)/n)/(sum(base)/n or 1):.2f}x")
