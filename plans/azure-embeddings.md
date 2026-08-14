# Plan v2: Azure embedding backend for wend's semantic index

Revised after review by Codex and three parallel reviewers (`rev-rust`, `rev-data`,
plus an over-engineering pass). Every claim below was verified against the source
or measured against the real index; corrections from round 1 are marked ✎.

## Why

Measured on the real corpus (500 real prompts from the live index, 34 Turkish
queries written to share no literal wording with their target doc):

| backend | recall@1 | recall@5 | MRR@10 |
|---|---:|---:|---:|
| Azure `text-embedding-3-large` @1024 | 73.5% | 85.3% | 0.771 |
| local `multilingual-e5-small` @384 (today's `semantic`) | 29.4% | 50.0% | 0.381 |
| BM25 (what ships today) | 0.0% | 0.0% | 0.000 |

BM25 scores zero because `search::compile_query` (`search/mod.rs:13`) ANDs every
term, so a natural-language query only matches a doc containing all of its words.
Out of scope here, but it is why the practical delta is 0.000 → 0.771.

Deployment is live and verified: `wend-embed-oai` → `text-embedding-3-large`,
SKU `GlobalStandard` (pay-per-token, no provisioned compute), Sweden Central.
`dimensions` is honoured (256/1024/1536/3072) and output is L2-normalized
(‖v‖ = 1.000±0.003), so the existing `dot()` stays a valid cosine.

Measured against the real index, not estimated: **20,124 chunks, 6.92 M tokens,
$0.90** one-time at $0.13/1M. Incremental runs embed only new chunks.

## Design

### 1. Module layout ✎ (was 3 files, now 2)

`embed/mod.rs` keeps chunking, `build_index`, `hybrid_search`, `dot`, `snippet`,
and the ~35-line fastembed backend inline under `#[cfg(feature = "semantic")]`.
Only the Azure backend gets its own file, because it is a genuinely separate
concern (HTTP, retry, JSON):

```
embed/mod.rs     shared + local backend      (any(semantic, azure))
embed/azure.rs   Azure REST backend          (feature = "azure")
```

Splitting the 35-line local backend into its own file too was cut as churn.

### 2. Backend selection

```rust
pub enum Embedder {
    #[cfg(feature = "semantic")]
    Local(TextEmbedding),
    #[cfg(feature = "azure")]
    Azure(azure::Azure),
}
```

Runtime selection is required (not compile-time), because `--features
semantic,azure` must work and the choice is per-invocation via the environment.

✎ **Every method must mirror the variant cfgs exactly.** Forgetting one arm
produces a non-exhaustive-match error only in single-feature builds, which is
precisely the combination CI does not currently exercise. The four methods are
`load()`, `embed_passages()`, `embed_query()`, `model_id()`; each is a `match
self` with one `#[cfg]`-gated arm per variant.

✎ **Partial configuration is a hard error, never a silent fallback.** This was a
critical finding: with both features compiled in, a typo'd or empty
`WEND_AZURE_KEY` would fall back to local, see every Azure vector as stale, and
overwrite all 20k of them with local vectors through the existing upsert
(`store/mod.rs:554`) — silently destroying $0.90 of work and degrading search
from 0.771 to 0.381. Rule: if *none* of the Azure vars are set, use local; if
*some but not all* are set, or any is empty, error out naming the missing one.

`model_id()` returns `multilingual-e5-small` (unchanged, so existing local
vectors stay valid) or `azure:<deployment>:<dims>`.

### 3. Model guard (correctness fix, required)

`all_chunk_vectors` (`store/mod.rs:569`) ignores the `model` column and `dot()`
(`embed/mod.rs:143`) zips to the shorter vector, so switching models today
silently produces garbage scores against every existing chunk.

- `all_chunk_vectors(model)` filters `WHERE v.model IS ?1`.
- `chunks_needing_vectors(model)` returns chunks with no vector **or** whose
  vector has `v.model IS NOT ?1`.

✎ Use SQLite's NULL-safe `IS` / `IS NOT`, not `=` / `<>`. A round-1 review
claimed legacy NULL-model rows exist; that was checked and is **false** —
`store_chunk_vector` (`store/mod.rs:552`) is the only writer of `chunk_vectors`
in the tree and `model: &str` was always required. `IS`/`IS NOT` is used anyway
because it is the same length and removes the failure class permanently.

✎ `dim` stays informational: `chunk_fk` is the PRIMARY KEY, one vector per
chunk, and dims are baked into the model string, so a dims change is always a
model change. But the Azure client **must** still verify each returned vector's
length equals the configured dims before storing — that check belongs at the
network boundary, not the query.

No migration needed; `model TEXT` already exists in v3 (`schema.rs:41`).
Regression test: two vectors under different model ids, assert the search path
sees only the current one.

### 4. Azure client (`embed/azure.rs`)

- Dependency: `ureq` 3. Already resolved in `Cargo.lock` at 3.3.0 via
  `fastembed → hf-hub`, so a `semantic,azure` build adds zero new crates. Avoids
  reqwest's tokio runtime, which this sync codebase has no other use for.
- `POST {endpoint}openai/v1/embeddings`, header `api-key`, body
  `{"model": deployment, "input": [...], "dimensions": 1024}`.
- ✎ **`.http_status_as_error(false)` is mandatory.** ureq 3 defaults it to true
  (`ureq-3.3.0/src/error.rs:9-14`), which collapses any 4xx/5xx into
  `Error::StatusCode(u16)` — a bare number with no headers. Under the default,
  reading `Retry-After` is literally impossible and the 429 handling silently
  never works.
- ✎ **An explicit global timeout is mandatory.** ureq 3 sets none by default
  (`ureq-3.3.0/src/error.rs:33-36`); one stalled connection would hang the whole
  backfill forever and never reach the retry path.
- ✎ **Reconstruct each batch by `data[].index` within that batch's own slice.**
  `index` is position within the single request's `input` array, not a global
  offset — indexing into the full pending list would misalign every batch after
  the first. Before any DB write, assert the response has exactly one unique,
  in-range index per input; reject gaps and duplicates. A silent mis-zip here
  attaches the wrong vector to the wrong chunk and is invisible afterwards.
- Retry: bounded exponential backoff. ✎ Azure OpenAI emits **`retry-after-ms`**
  as well as `Retry-After`; prefer the former and fall back to the latter, then
  to exponential backoff. Honouring only `Retry-After` means a 30-second
  throttle gets short retries and exhausts the attempt budget before recovery.
- Config, env only, never persisted, never logged. ✎ Three vars, all required
  when using Azure (`WEND_AZURE_DIMS` was cut — a knob nobody turns; 1024 is a
  const, chosen because brute-force search loads every vector into RAM per query:
  72 MB at 1024-d vs 216 MB at 3072-d, for a quality difference inside noise).
  - `WEND_AZURE_ENDPOINT`, `WEND_AZURE_KEY`, `WEND_AZURE_DEPLOYMENT`

### 5. Secret redaction before upload (required, evidence-backed)

Scanning the actual outbound corpus (11,870 real prompts that pass
`semantic_messages`'s filter, `store/mod.rs:466`) found, in plain text:

| pattern | messages |
|---|---:|
| AWS access key (`AKIA…`) | 4 |
| `SECRET`/`TOKEN`/`PASSWORD`/`API_KEY` assignments | 110 |
| Bearer tokens | 15 |
| DB URIs with inline credentials | 2 |
| JWT | 1 |

One message contains an AWS access key ID and its secret access key together.
`semantic_messages` filters by message *shape* (typed prose, not tool output);
it has no notion of secrets and cannot acquire one. So an unredacted design
uploads live credentials to a third party by construction.

✎ Redaction lives **inside `azure.rs`, applied to every string entering a
request body** — not in `build_index`. Two reasons, both from review:

1. `embed_query` is outbound too (`embed/mod.rs:169`). Redacting only chunk text
   means `wend search --semantic "why is AKIA… rejected"` ships the credential
   verbatim. Putting the pass at the HTTP boundary covers passages and queries
   with one call site that cannot be forgotten.
2. ✎ Whole-block PEM matching is **boundary-unsafe**. A >1200-byte private key
   is split by `chunk_texts` so that no single chunk holds both `BEGIN` and
   `END`, and a conventional `BEGIN…END` matcher silently passes the body
   through. So the PEM rule must be marker-optional in **both** directions:
   redact from a `BEGIN … PRIVATE KEY` marker to end-of-input; ✎ redact from
   start-of-input through an `END … PRIVATE KEY` marker when no opening marker
   is present (otherwise the key's tail chunk leaks, including a final fragment
   too short to trip the length rule); and redact any standalone run of ≥64
   base64 characters, which is what the middle continuation chunks look like.

High-confidence patterns only, to limit collateral damage to retrieval quality:
`AKIA[0-9A-Z]{16}`, `sk-`/`rk_live`/`pk_live`-style prefixed keys, vendor tokens
with known prefixes (`ghp_`/`github_pat_`/`glpat-`/`npm_`/`xox?-`/`AIza`/`SG.`),
bare JWTs, `Bearer <token>`, the PEM rule above, URIs with inline credentials,
and `KEY=VALUE` where the key name contains SECRET/TOKEN/PASSWORD/API_KEY.

✎ The vendor-token and bare-JWT rules were **added after measuring**: the first
pattern set was run over the real corpus and left GitHub tokens (4 messages),
npm tokens (3) and a bare JWT (1) untouched. With them, all 29 detectable
secrets in that corpus are redacted and 0 leak, while only 0.3% of ordinary
prompts (6 of 2000 sampled) are altered at all.

Note for implementation: Rust's `regex` does not let `.` match `\n` by default,
so the multi-line PEM rules need `(?s)`.

The stored chunk text (used for result snippets) is left intact — redaction
affects only what leaves the machine. Secrets carry no useful semantics, so
scrubbing them should not measurably hurt recall; the eval is re-run with
redaction on to confirm that.

This is not gold-plating: it is validation at a trust boundary, on evidence.

### 6. Backfill throughput ✎

✎ The round-1 arithmetic was wrong (it said ~700 requests). Measured: 20,124
chunks, avg 344 tokens, 6.92 M tokens total → **210 requests at batch size 96**,
~33 k tokens each.

✎ The deployment's capacity was raised from 500 to **2000 units (2 M TPM)**, so
a proactive rate limiter is not needed: the token floor is 6.92M / 2M = 3.5 min
against ~3.5–7 min of sequential request latency for 210 requests.

✎ That margin is thin (~1% at the fast end), and Azure meters on its own
character-based estimate, so **throttling is expected, not exceptional**. The
retry path is therefore load-bearing and must be robust rather than a rare
fallback — that is why it gets its own unit test against a stub. No proactive
limiter and no concurrency are added; if measured wall-clock proves bad, add
pacing then, not speculatively.

✎ **Measured on the real backfill**, both estimates above turned out wrong in a
way that matters: throughput is ~1150 vectors/min (~12 requests/min, ~5 s per
96-chunk request), so 20,388 chunks take **~18 minutes**, not 4–7. The binding
constraint is Azure's per-request latency, not the token ceiling — which
confirms the decision not to add a rate limiter, and means the only lever that
would help is concurrency. Not worth it for a once-per-machine backfill;
revisit if incremental runs ever get large.

✎ The token estimator counts **UTF-8 bytes / 3.17** (measured on the real
corpus), not chars. Measured chunk sizes: mean 1090 B, max 2431 B (≈767 tokens),
far under the model's 8191-token input limit.

✎ `Store.conn` is private (`store/mod.rs:108`), so `build_index` (a different
module) cannot open a transaction. Add one `Store` method that writes a whole
batch inside a single transaction, replacing the per-row `store_chunk_vector`
and its single caller. The transaction wraps **only** the DB writes, never the
network call or its backoff — holding a write txn open across a multi-second
retry buys nothing.

Resume safety for *vectors* is unchanged: `build_index` drives off
`chunks_needing_vectors`, which now also covers model changes. A crash mid-batch
rolls that batch back and re-embeds it next run (cost: cents).

✎ Resume safety for *chunk creation* is currently broken and is fixed here.
`build_index` inserts chunks one row at a time (`store/mod.rs:527`) while
`sessions_without_chunks` (`store/mod.rs:451`) only finds sessions with **zero**
chunks. A crash after the first insert of a session leaves that session
permanently half-chunked: it never reappears as needing work, so the rest of its
messages are silently unsearchable forever. Each session's chunk set must be
inserted in one transaction.

✎ This needs a second new `Store` method (`insert_session_chunks`), for the same
reason as the vector batch method: `conn` is private and `insert_chunk`
(`store/mod.rs:527`) autocommits per row, so the fix is not expressible through
today's API. So `Store` gains exactly two methods and loses one:
`insert_session_chunks` and `store_chunk_vectors_batch` replace `insert_chunk`
and `store_chunk_vector`.

### 7. Fix `chunk_texts`'s byte/char inconsistency (pulled into scope) ✎

`chunk_texts` tests `m.len() > CHUNK_BYTES` in **bytes** (`embed/mod.rs:87`) then
hard-splits on `CHUNK_BYTES` **chars** (`embed/mod.rs:94`). Measured on the real
corpus: **12,934 of 20,124 chunks (64.3%) exceed the 1200-byte target**, max
2431 B. The comment at `embed/mod.rs:18-19` claims chunks stay under e5's
512-token limit; at 2431 B ≈ 767 tokens that is false for most chunks, and
fastembed silently truncates — a plausible contributor to local e5's 0.381.

Harmless for Azure (8191-token limit), but it is pulled into scope for one
reason: this change creates all 20k chunks for the first time. Creating them
with a known-broken chunker means re-chunking and re-paying $0.90 later. The fix
is one line (split on byte length at a char boundary).

✎ Fixing the chunker does **not** repair chunks already in a user's DB:
`sessions_without_chunks` only re-chunks sessions with zero chunk rows, so
anyone who ran the local `semantic` build keeps their oversized chunks forever.
Rather than add chunker-version machinery, schema **v4** wipes the chunk tables
and forces exactly one rebuild. `SCHEMA_VERSION` goes to 4. Two details, both
from review:

✎ v4 must delete **both** tables explicitly — `DELETE FROM chunk_vectors;
DELETE FROM chunks;` — not rely on the cascade. `foreign_keys` is enabled only
in `Store::open` (`store/mod.rs:136`), never inside the public `migrate()`
(`schema.rs:143`), which the existing tests call directly on a bare connection.
Without the pragma the cascade does not fire, leaving orphaned vectors whose
`chunk_fk` the rebuilt chunks can reuse — silently attaching a stale vector to
unrelated text.

✎ `migrate()` writes `user_version` unconditionally (`schema.rs:154`), so a v3
binary opening a v4 DB **downgrades it to 3**, and the next v4 binary re-runs the
destructive migration and charges another $0.90. Fix: error out when
`version > SCHEMA_VERSION`, and only write `user_version` when it increases.

### 8. CLI surface ✎

No new commands, but the wiring is currently `semantic`-only and must change:

- `wend-cli/Cargo.toml:14` gains `azure = ["wend-core/azure"]`.
- `lib.rs:8`, `main.rs:364`, `main.rs:376`, `main.rs:382`, `main.rs:387` become
  `any(feature = "semantic", feature = "azure")`. Left as-is, an azure-only
  build compiles but every `--embed` / `--semantic` falls into the `not(semantic)`
  arm and prints "rebuild with --features semantic" while never calling Azure.
- `main.rs:134`'s `cfg!(feature = "semantic")` in `doctor` prints the wrong
  message for an azure build; report the active backend and model id instead.
- `main.rs:366`'s CPU-thread message is local-only and must not print for Azure.
- Before a backfill over ~1000 chunks, print the estimated token count and USD
  cost (one `println!`, not a subsystem).

### 9. Docs

`README.md:4` says "Fast, local, single binary, zero network." That stays true
for the default build. State plainly that `--features azure` adds an opt-in
remote backend that sends the user's own prompt text (secret-redacted) to their
own Azure resource, and that it is inert unless all three env vars are set.

## Explicitly not in scope

- `compile_query`'s AND-ing (BM25 = 0%). Real, worth fixing, separate risk
  surface. RRF already tolerates an empty keyword side.
- Making `--semantic` the default for `wend search`.
- ANN indexing. 20k × 1024-d f32 = 82 MB per query is fine; revisit past ~100k.
- Dropping the local fastembed backend.
- Letting local and Azure vectors coexist. `chunk_fk` is the PRIMARY KEY, so
  flapping between backends means a full re-embed each way. Documented, not fixed.

## Verification (all must pass)

1. `cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D warnings`;
   `cargo test --workspace` for: default, `--features semantic`, `--features
   azure`, and ✎ **`--features semantic,azure`** (the combination that catches
   cfg-mirroring mistakes, and the one CI does not run today).
2. Unit: model guard (two model ids, only current returned); batch splitting;
   per-batch index reconstruction incl. rejection of gaps/duplicates; 429 retry
   against a stub; redaction patterns; chunk sizes now ≤1200 bytes.
3. Live end-to-end on the real index: `wend index --embed` over all 20k chunks,
   then `wend search --semantic` on Turkish queries with known answers.
4. Re-run the 34-query eval **through the shipped binary**, with redaction on,
   and confirm MRR ≈ 0.77.
5. Default build still compiles with no network dependency; `wend search`
   unchanged; musl static smoke-build still passes.
