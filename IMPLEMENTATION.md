# Implementation Plan (buildable) — v2

Granular, file-level build plan. Companion to `PLAN.md` (strategy/phases) and `ARCHITECTURE.md` (tech stack). This is the document you build from. Code/identifiers in English; grounded in two review iterations (5 agents + Codex each, web + real-corpus verified). ⚠ marks a correction.

---

## 0. Decisions & corrections (read first)
- ⚠ **Subagent scope (was unspecified — now PINNED):** by default the indexer indexes **top-level sessions only** for *search* (~206 files / ~0.74 GB → all perf/size numbers below assume this). The **~6,040 subagent `.jsonl`** are parsed for **topology only** (relations/workflows), NOT into `messages_fts`, unless the user runs `index --include-subagents` (a separate, larger pass). This keeps the default fast and the index small.
- ⚠ **Glob disambiguation (explicit rule):** top-level session = a `.jsonl` exactly **one** path segment below the encoded-project dir (`projects/<enc>/<sid>.jsonl`). Anything under `<sid>/` (i.e. `subagents/…`, `workflows/…`) is subagent/workflow. The top-level glob must be single-level (not recursive).
- ⚠ **Forest roots (corrected, real-data verified):** roots = **ALL** lines with `parent_uuid == null` (conversation roots **and** `system/compact_boundary` nodes). The DFS must **traverse every node type** (`user`, `assistant`, `attachment`, `progress`, …) — restricting to user/assistant lost 1471/1555 messages in a 10-boundary file. (This reverses the v1 §0 note.)
- ⚠ **Content lives in `message.content`** for `user`/`assistant` (string or array of blocks). `obj.content` (top-level) only appears on meta lines like `system/compact_boundary`. Parser must read `obj.message.content`.
- ⚠ **Store every conversation/graph node** (with `uuid`, `parent_uuid`, `content_json`) even if its FTS text is empty (image-only/thinking-only) — needed for forest reconstruction + `show`/`export`. Only **omit empty text from the FTS index**, never drop the row.
- ⚠ **sqlite-vec registration** uses the real 3-arg SQLite entry-point signature: `unsafe extern "C" fn(*mut sqlite3, *mut *mut c_char, *const sqlite3_api_routines) -> c_int`. Copy the pattern from the `sqlite-vec` crate's own example, or use `rusqlite::auto_extension::register_auto_extension`. Register **before** opening any connection/pool. (The README's `fn() -> i32` transmute is wrong.)
- ⚠ **rusqlite + r2d2_sqlite version coupling (build-breaker if wrong):** `~0.38` does NOT exist, and `r2d2_sqlite 0.34` depends on `rusqlite ^0.39` (= `<0.40`), so it **cannot** be paired with `rusqlite 0.40` — Cargo won't unify, duplicate `libsqlite3-sys`/`Connection` types break the build. **Decision (pick at S0 after `cargo tree -d`):** (A) pin `rusqlite = 0.39.x` to match `r2d2_sqlite 0.34` and import rusqlite via `r2d2_sqlite::rusqlite` everywhere (one version, simplest); **or** (B) stay on `rusqlite 0.40` and drop r2d2_sqlite for a tiny hand-rolled read-only pool (`Mutex<Vec<Connection>>` or per-query read-only `Connection` — cheap under WAL). Default to (A). Needs rusqlite ≥0.34 for the ffi auto-extension API (both 0.39/0.40 qualify).
- ⚠ **Embeddings are f32** (default). bge-small-en-v1.5 weights are **~130 MB f32** (the "33 MB"/"int8" figures in ARCHITECTURE §3/§4 are wrong — 33 M is the *param count*; int8 BERT in Candle is immature/often slower on CPU). int8 is a *possible* later optimization, not the plan of record. **ARCHITECTURE §3/§4 to be corrected.**
- ⚠ **Candle ⇒ `onig` C dep on musl:** `candle-core` force-enables `tokenizers`'s `onig` feature (→ vendored Oniguruma C via `cc`) on all non-wasm targets; it's *static* (no system lib) but the **musl build of the `semantic` feature needs a musl C cross-compiler** (muslrust/cross). ⚠ Note the **default binary is NOT literally "pure Rust" either** — it bundles SQLite + sqlite-vec **C** (compiled via `cc`), so the core musl build also needs a C cross-compiler. The distinction: the core's plain C (SQLite amalgamation + sqlite-vec) static-links cleanly and reliably (covered by the S0 musl smoke build); the `semantic` feature adds the finickier `onig` C. **Neither needs a *system* library at runtime** — that's the "runs everywhere" promise (no runtime deps), achieved with a C cross-compiler at build time.
- ⚠ **cargo-dist**: use **upstream `axodotdev/cargo-dist`** (canonical, active through v0.32.0; it re-merged Astral's temporary fork). (Drop any "astral fork archived" rationale — the conclusion stands, the reason was wrong.)
- ⚠ **Paths**: `etcetera` (`choose_app_strategy`) — XDG on Linux/macOS + Known Folders on Windows.
- ⚠ **Dir encoding**: `/`→`-` and `.`→`-`. Lossy → always **cross-validate** a reverse-encoded candidate dir by reading the `cwd` field inside its files (§5).

---

## 1. Workspace layout (lib + bin)
```
session-recall/
├── Cargo.toml                  # [workspace] + [workspace.dependencies]
├── rust-toolchain.toml         # pin stable + rustfmt, clippy; pin zig version for CI
├── deny.toml                   # cargo-deny
├── .github/workflows/{ci.yml,release.yml}
├── fixtures/                   # deterministic Phase-0 corpus (committed, redacted, minimized)
├── crates/
│   ├── recall-core/            # pure engine — NO clap/ratatui, NO stdout. thiserror only.
│   │   └── src/{lib,error,config,model}.rs
│   │       parser/{mod,content,routing}.rs
│   │       store/{mod,schema,writer,reader,query}.rs   # query.rs = FTS MATCH compiler/escaper
│   │       index/mod.rs · search/mod.rs · recover/mod.rs · topology/mod.rs
│   │       embed/mod.rs        # feature="semantic"
│   ├── recall-cli/             # binary `recall`
│   │   └── src/{main,cli,logging}.rs · commands/* · render/* · tui/*  (tui feature)
│   └── recall-skill/           # later: helpers shared with plugin
└── plugin/                     # Claude Code plugin (Phase 7)
    └── .claude-plugin/plugin.json · skills/find/SKILL.md · commands/search.md · hooks/hooks.json · bin/
```
`thiserror` in core, `anyhow`+context in CLI → explicit exit codes. `tracing` → **stderr only**. **Feature flags:** `default = []` (lean, headless/plugin-friendly); `tui` (nucleo+ratatui) opt-in; `semantic` opt-in; `fast-embed` (ort) opt-in, NEVER in `--all-features` CI.

---

## 2. Database schema (DDL — v2)
**PRAGMAs split by role.** Writer/migration connection (once): `PRAGMA journal_mode=WAL` (persistent). Every connection (writer + each pooled reader), connection-local: `synchronous=NORMAL` (writer), `cache_size=-262144`, `mmap_size=268435456`, `temp_store=MEMORY`, `busy_timeout=5000`. Readers open with `OpenFlags::SQLITE_OPEN_READ_ONLY` and set `query_only=ON`; do **not** set `journal_mode` on readers. Use `conn.pragma_update(...)` (not `execute_batch`, which mis-handles row-returning `journal_mode`). Create `index.db` with mode `0o600` (Unix) before open.

```sql
PRAGMA user_version;   -- migration gate

CREATE TABLE session_files(
  path TEXT PRIMARY KEY, source_kind TEXT, head_tail_hash TEXT,
  mtime_ns INTEGER, size INTEGER, last_byte_offset INTEGER,
  parser_version INTEGER, scan_started_at INTEGER, scan_finished_at INTEGER);

CREATE TABLE sessions(
  id INTEGER PRIMARY KEY, session_id TEXT, source_kind TEXT, file_path TEXT,
  project_path TEXT, project_name TEXT, git_branch TEXT,
  first_ts INTEGER, last_ts INTEGER, ai_title TEXT, custom_title TEXT,
  title TEXT,   -- = coalesce(custom_title, ai_title, ''); the content column for sessions_fts (set by indexer/`name`)
  message_count INTEGER, has_compaction INTEGER, indexed_at INTEGER,
  UNIQUE(source_kind, session_id, file_path));

CREATE TABLE messages(
  id INTEGER PRIMARY KEY,
  session_fk INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  uuid TEXT, parent_uuid TEXT, line_no INTEGER, source_file TEXT,
  type TEXT, subtype TEXT, role TEXT, ts INTEGER, cwd TEXT,
  content_json TEXT,        -- normalized renderable blocks (ALWAYS stored)
  text_for_fts TEXT,        -- flattened body; '' allowed (then not FTS-indexed)
  is_sidechain INTEGER, is_compact_summary INTEGER);
CREATE INDEX idx_messages_session ON messages(session_fk);
CREATE INDEX idx_messages_uuid ON messages(uuid);

CREATE TABLE boundaries(
  id INTEGER PRIMARY KEY, session_fk INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
  uuid TEXT, parent_uuid TEXT, logical_parent_uuid TEXT, logical_parent_file TEXT,
  trigger TEXT, pre_tokens INTEGER, post_tokens INTEGER, ts INTEGER);

CREATE TABLE boundary_messages(
  boundary_fk INTEGER REFERENCES boundaries(id) ON DELETE CASCADE,
  message_fk INTEGER REFERENCES messages(id) ON DELETE CASCADE,
  path_order INTEGER, distance INTEGER, source TEXT);

CREATE TABLE relations(
  parent_fk INTEGER, child_fk INTEGER, relation_type TEXT,
  evidence TEXT, confidence TEXT, source_path TEXT, tool_use_id TEXT, workflow_id TEXT);

CREATE TABLE workflows(id INTEGER PRIMARY KEY, parent_session_fk INTEGER REFERENCES sessions(id) ON DELETE CASCADE, wf_id TEXT, meta_path TEXT);
CREATE TABLE workflow_events(workflow_fk INTEGER REFERENCES workflows(id) ON DELETE CASCADE, kind TEXT, ts INTEGER, payload TEXT);

CREATE TABLE worktrees(
  session_fk INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
  original_cwd TEXT, worktree_path TEXT, worktree_name TEXT,
  branch TEXT, original_branch TEXT, original_head TEXT,
  continues_session_id TEXT, link_confidence TEXT);

-- embeddings are CHUNK-level (a chunk may span/split messages):
CREATE TABLE chunks(
  id INTEGER PRIMARY KEY,
  message_fk INTEGER REFERENCES messages(id) ON DELETE CASCADE,
  session_fk INTEGER, ordinal INTEGER, token_count INTEGER, text TEXT);

-- FTS5 external-content over messages.text_for_fts + sync triggers
CREATE VIRTUAL TABLE messages_fts USING fts5(
  text_for_fts, content='messages', content_rowid='id',
  tokenize='porter unicode61 remove_diacritics 1');
CREATE TRIGGER messages_ai AFTER INSERT ON messages WHEN new.text_for_fts <> '' BEGIN
  INSERT INTO messages_fts(rowid, text_for_fts) VALUES (new.id, new.text_for_fts); END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages WHEN old.text_for_fts <> '' BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, text_for_fts) VALUES('delete', old.id, old.text_for_fts); END;
CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN   -- guarded both sides: never touch FTS for empty text
  INSERT INTO messages_fts(messages_fts, rowid, text_for_fts)
    SELECT 'delete', old.id, old.text_for_fts WHERE old.text_for_fts <> '';
  INSERT INTO messages_fts(rowid, text_for_fts)
    SELECT new.id, new.text_for_fts WHERE new.text_for_fts <> ''; END;

-- sessions title/alias FTS: EXTERNAL-content over sessions.title (real column above, so snippet() works)
CREATE VIRTUAL TABLE sessions_fts USING fts5(title, content='sessions', content_rowid='id');
CREATE TRIGGER sessions_ai AFTER INSERT ON sessions WHEN new.title <> '' BEGIN
  INSERT INTO sessions_fts(rowid, title) VALUES (new.id, new.title); END;
CREATE TRIGGER sessions_ad AFTER DELETE ON sessions WHEN old.title <> '' BEGIN
  INSERT INTO sessions_fts(sessions_fts, rowid, title) VALUES('delete', old.id, old.title); END;
CREATE TRIGGER sessions_au AFTER UPDATE ON sessions BEGIN   -- `name` updates title → keep FTS in sync automatically
  INSERT INTO sessions_fts(sessions_fts, rowid, title) SELECT 'delete', old.id, old.title WHERE old.title <> '';
  INSERT INTO sessions_fts(rowid, title) SELECT new.id, new.title WHERE new.title <> ''; END;

-- semantic feature only — keyed by CHUNK, not message:
CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[384]);
```
**External-content discipline:** `'delete'` rows pass **old column values** (FTS5 re-tokenizes to subtract). After any trigger-bypassing bulk path: `INSERT INTO messages_fts(messages_fts) VALUES('rebuild');`. Verify with `('integrity-check', 1)`.

**Per-file replacement (idempotent reindex — the core of incremental):** in ONE transaction per file: `DELETE FROM sessions WHERE file_path=?1` (CASCADE clears messages/boundaries/boundary_messages/chunks/workflows/worktrees; triggers clear FTS; explicitly `DELETE FROM vec_chunks WHERE chunk_id IN (…)` and `DELETE FROM relations WHERE source_path=?1` since those lack FK cascade), then re-insert. `PRAGMA foreign_keys=ON` required for CASCADE — set it on **every** connection (writer + each pooled reader), not just the migration connection. Test: index same fixture 3×, then mutate/truncate → assert no duplicate messages, no orphan FTS/vec rows, `PRAGMA foreign_key_check` clean.

**Query input safety:** never pass raw user text to `MATCH`. `store/query.rs` compiles user input into a safe FTS5 query (quote bare terms, strip/escape `"`, `:`, `-`, `*`, `(`, `)`, `AND/OR/NOT` unless an "advanced" mode is requested). KNN: `WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance` (k required). Bind `&[f32]` as BLOB via `bytemuck::cast_slice` (little-endian; all targets LE — add a code comment).

**Connections:** one writer `Connection` (serialize via a named cross-platform **file lock**, `fs4`/`fd-lock`, + `busy_timeout`); `r2d2_sqlite` **read-only** pool (each reader re-applies connection-local PRAGMAs). Register sqlite-vec auto-extension before opening the pool.

**Timestamps:** parse RFC3339/ISO-8601 (`time`/`chrono`) → store epoch-ms `INTEGER` (UTC); missing ts → fall back to file order; tie-break by `line_no`.

---

## 3. Parsing & flattening (`parser/`)
Globs per §0. Parse with `sonic-rs` (lazy pointer-get), `serde_json` fallback. mmap + split `\n`. ⚠ **Ordered reduction:** `rayon` may decode lines in parallel, but each file emits `(line_no, record)` which is **sorted by `line_no` before** writing session metadata / boundary order / recovery rows (latest-title, first-`cwd`, boundary order all depend on file order). Per-line JSON failure → skip+`warn`, file txn stays atomic.

**Content location by type** (⚠ not all types use `message.content`): `user`/`assistant` → `obj.message.content`; `attachment` → `obj.attachment.content` (subtype `file` → `obj.attachment.content.file.content`; `compact_file_reference` → no content → FTS `''`); `progress` → `obj.data` (FTS `''`); `system/*`/`boundary` → no body. ⚠ `obj.isCompactSummary` is **top-level** (not under `obj.message`).

**Content-block flattener** (for the array/string shapes found under `message.content`) → `(fts_text, Vec<NormalizedBlock>)`:
| shape | fts_text | normalized |
|---|---|---|
| string | the string | `{kind:text,text}` |
| `text` | `.text` | as-is |
| `thinking` | `""` | `{kind:thinking,text}` (kept) |
| `tool_use` | `.name + scalars(.input)` | `{kind:tool_use,id,name,input}` |
| `tool_result` | recurse `.content` (string→text; arr: text→`.text`, image→`""`, tool_reference→`.tool_name`) | keeps `tool_use_id,is_error`+children |
| `image` | `""` | `{kind:image,media_type,byte_len}` — never store base64 |
FTS body = newline-join of non-empty parts; may be `''`. **The message row is always stored** (content_json + uuid/parent) even when FTS body is empty.

**Routing** (`type`/`subtype` → table/behavior):
- `user`,`assistant` → **messages** (read `message.content`, `message.role`; cache `cwd` from first line that has one — fall back to dir-decode if none).
- `attachment` (~21.8k, part of graph) → **messages** node (traverse; content at `obj.attachment.content`; `file` subtype → FTS text; `compact_file_reference`/others → store node, empty FTS).
- `progress` (~5.4k, has `parentUuid`+`cwd`) → **messages** node (traverse; content at `obj.data`; empty FTS).
- `system`+`compact_boundary` → **boundaries**.
- ⚠ **all other `system/*` → TRAVERSE (pass-through): store `uuid`+`parent_uuid` in `messages` (empty FTS, no separate table); NEVER stop DFS here.** Includes `turn_duration`, `stop_hook_summary`, `api_error`, `away_summary`, `local_command`, `scheduled_task_fire`, `bridge_status` — these parent real `user`/`assistant` turns (stopping loses 27–72% of messages on real files).
- `worktree-state` → **worktrees**. `bridge-session` → **relations**(bridge).
- `ai-title`(latest),`custom-title` → **sessions** metadata. `agent-name`,`last-prompt`,`permission-mode`,`mode`,`pr-link`,`file-history-snapshot`,`queue-operation`,`agent-setting` → skip (no graph role).
- subagent `.meta.json` → **workflows/relations** (`agentType` always; null-guard `name`,`toolUseId`).

---

## 4. Compaction recovery (`recover/`)
1. Load lines → `uuid→record` map + children adjacency from `parent_uuid`.
2. Boundaries = `system/compact_boundary`, ordered by `line_no` (file order ≈ chronological, verified monotonic).
3. ⚠ Roots = **ALL** records with `parent_uuid==null`; order by `ts` then `line_no`.
4. DFS each root over children following `parent_uuid`, **traversing all node types** → ordered chains.
5. Bridge: per boundary resolve `logical_parent_uuid` → splice pre-compaction chain (ending at that uuid) before the post-boundary chain (the `isCompactSummary:true` user line — ⚠ field is **top-level** `obj.isCompactSummary`, not under `obj.message` — is parented to the boundary uuid).
6. Cross-file (~0.3%): if `logical_parent_uuid` not in-file → search sibling `*.jsonl` in same dir; record `logical_parent_file`. ⚠ If still unresolved → mark boundary **orphan**, emit the recoverable partial transcript + a `tracing::warn` + a user-visible "earliest history unrecoverable (source file missing)" note. Never silently truncate.
7. Emit ordered transcript; fill `boundary_messages(path_order,distance,source)`.
**Output cap:** default = write full transcript to file, return path + bounded header (≤~10 boundaries, paginate); inline ≤ **50 KB / ~800 lines** via `--range/--head/--tail`. Wrap in untrusted-data delimiters.

---

## 5. Topology (`topology/`)
Reverse-encode `original_cwd` (`/`→`-`,`.`→`-`) → candidate dir; ⚠ **cross-validate** by reading the `cwd` field in candidate files (encoding is lossy/ambiguous). Candidates = files in that dir **and** the worktree's own dir (a worktree session can continue a prior *sibling*). Rank: (a) explicit `continues_session_id` (high); (b) `original_branch`+`original_head` match (med); (c) nearest preceding `last_ts` in window (low). Label every edge explicit/inferred + confidence. Ghost dirs (no `.jsonl`) → node confidence=none, never crash. `tree` renders repo → worktrees → subagent/workflow children.

---

## 6. Search & ranking (`search/`)
- Keyword: FTS5 bm25; merge `sessions_fts` (title/alias) + `messages_fts` hits → ranked **session** list (best snippet). Compile/escape input (§2).
- Composite: `score = base × recency_decay(≈30-day half-life, cap +20%) × active_project_boost`.
- Hybrid (semantic on): BM25 top-N (by message) + vec KNN top-N (by **chunk**) → roll chunk hits up to their `message_fk`→`session` → fuse with **RRF k=60** (`Σ 1/(k+rank)`; ranks unify bm25-asc with vec-distance-asc, no normalization).

---

## 7. Embeddings (`embed/`, feature="semantic")
Crates (pin at S0): `candle-core`/`candle-nn`/`candle-transformers` (0.10.x), `tokenizers`, `hf-hub` **0.5.0** (or use `ApiBuilder::new().with_cache_dir(cache_dir()).build()` — ⚠ on hf-hub 1.0 `Api::new()` ignores `HF_HOME`), `default-features=false, features=["rustls-tls","tokio"]` (avoid OpenSSL on musl). Load `BAAI/bge-small-en-v1.5` CPU; chunk transcripts into the `chunks` table (~500 tok / ~15% overlap, turn-aligned; a chunk may span/split messages → keyed by `chunk.id`, FK to a representative `message`). Tokenize (truncate 512, pad, attention_mask, zeros token_type_ids) → BERT forward → **CLS pooling (token 0)** → L2-normalize → 384-d f32 → `vec_chunks` BLOB. Query prefix `"Represent this sentence for searching relevant passages: "` on **queries only**. Throughput ~30–80 texts/s/core → opt-in, incremental, resumable, `indicatif` progress, never blocks keyword tier. ⚠ musl build of this feature needs a musl C toolchain (onig); document in README + CI uses muslrust/cross for the `semantic` job.

---

## 8. CLI / UX (`recall-cli`)
`clap` derive. ⚠ **Display-id model:** the canonical user-facing id is a **short prefix of the session UUID** (e.g. 8 chars), since `session_id` is unique per top-level file in practice; on collision/ambiguity `show`/`resume`/`name`/`export` print the matching candidates (project + date) and ask the user to disambiguate (longer prefix). Internal PK is never shown. Commands: `index [--embed] [--incremental] [--include-subagents]`, `search <q> [--semantic] [--json] [--since] [--project] [--limit]`, `show <id> [--recovered] [--range/--head/--tail] [--json]`, `tree [project] [--json]`, `resume <id>` (verify cwd exists; emit `cd <cwd> && claude --resume <id>`, never exec), `export <id> --format md|html|json` (renders `content_json`, honors recovery; redaction best-effort/opt-in/preview; `.gitignore` warning), `name <id> <alias>` (writes `custom_title` + syncs `sessions_fts`), `doctor` (DB integrity, SIMD, dirs, model/network/index state, perms). Interactive picker (`tui` feature): `nucleo`+`ratatui`. Output: `comfy-table`, `nu-ansi-term`, OSC-8, `--json`.

---

## 9. Plugin (`plugin/`, Phase 7)
- `plugin.json` (`name:"recall"`); `bin/` on PATH; `${CLAUDE_PLUGIN_ROOT}` for hook paths.
- **NL skill** `skills/find/SKILL.md`: always-on; tight directive English `description` (≤1,536 chars w/ `when_to_use`; 1–2 Turkish phrases; avoid generic "nerede konuştuk"); `allowed-tools: ["Bash(recall *)"]` — **no exec/Edit/Write** (this is the real injection control; the `<untrusted-data>` wrapper is secondary/soft).
- ⚠ **Explicit search = a COMMAND, not a `disable-model-invocation` skill.** `commands/search.md` (a slash command `/recall:search`) — because issue #26251 shows `disable-model-invocation:true` can block even slash-invocation of a skill. S8 gate must confirm `/recall:search` actually runs; fallback = rely on the `find` skill alone.
- **Hook** `hooks/hooks.json`: `SessionStart`(startup)+`UserPromptSubmit` → `bin/hook-index-check.sh`: cheap stat (index age); if stale spawn `nohup recall index --incremental </dev/null >/dev/null 2>&1 & disown`. ⚠ **Must close ALL three FDs incl. stdin (`</dev/null`)** — a background process inheriting the parent's stdin keeps the stream-json pipe open and hangs Claude Code indefinitely after v2.1.87 (#43123). Silent on stdout (it's injected into context; SessionStart blocks and async doesn't apply → detached is the only safe form).

---

## 10. Build sequence (≈3 weeks + ~30% buffer for the 2 unproven risks)
| Step | Area | Deliverable | Gate |
|---|---|---|---|
| S0 | repo | workspace; CI (fmt/clippy -Dwarnings/test, **split feature matrix**, no fast-embed in all-features); `cargo dist init`; **early musl smoke-build of bundled-C core (sqlite+sqlite-vec, no candle)** asserting static via `ldd` | green CI all targets |
| S1 (Ph0) | fixtures+`parser` | minimizer→`fixtures/`; flattener+routing (incl attachment/progress); ordered reduction; `insta` golden (cross-file lpu, multi-root, base64, workflow-meta) | snapshots stable |
| S2 (Ph1) | `store`+`index` | schema+migrations; PRAGMA split; FTS triggers; query compiler; **idempotent per-file replacement**; file lock | index top-level corpus cold <~5 s; 0600; reindex-3×+mutate → no dupes/orphans, `foreign_key_check` clean |
| S3 (Ph2) | `search`+cli | search/show/resume/doctor/name/export + `--json`; display-id disambiguation | known-item top-3; missing-cwd handled |
| S4 (Ph3) | `recover` | forest(all-roots)+bridge+cross-file(+orphan); caps; untrusted wrap | recovered==raw survivors on 10-boundary file; boundary-relative |
| S5 (Ph4) | `topology` | `tree`+resolver(+cwd cross-validate)+confidence+ghost dirs | correct on known worktree case |
| S6 (Ph7-lite) | release | `cargo dist`(axodotdev) binaries via zigbuild(universal2)+cross(musl); rcodesign **notarize** (no staple on bare binary); brew/curl | binary runs on clean no-toolchain machine |
| S7 (fast-follow) | `embed` | semantic: candle(CLS,f32)+chunks+vec_chunks+RRF; musl job uses muslrust | hybrid beats keyword on labeled set |
| S8 (fast-follow) | `plugin` | find skill + `/recall:search` command + hook; `claude plugin validate` | find fires on intended not generic; **`/recall:search` confirmed invokable** |
Cloud (Ph9) parked.

---

## 11. Testing & CI
- `insta` snapshots (parser/recover/topology). Hermetic `tempfile` DBs over `fixtures/` — no `~/.claude` dep.
- **Idempotency test:** index fixture 3× + mutate + truncate → no dup messages, no orphan FTS/vec/relations rows, `PRAGMA foreign_key_check` clean.
- FTS drift: insert→update→delete→truncate → `integrity-check`.
- CLI: `assert_cmd`+`predicates`+`trycmd` (`--help`/`--json`).
- Bench: `criterion` cold-index + search latency.
- CI matrix: ubuntu(gnu+musl)/macos/windows × stable; fmt, clippy `-Dwarnings`, test (**no fast-embed**), build all targets; assert musl binary static (`ldd`); `cargo-deny`. `release.yml` via `cargo dist`; zigbuild universal2 + cross musl; rcodesign notarize from Linux.

---

## 12. Dependency pins (set exact at S0)
`rusqlite{bundled}` **0.39.x** + `r2d2` + `r2d2_sqlite` 0.34 (import rusqlite via `r2d2_sqlite::rusqlite`; ONE `libsqlite3-sys` — verify `cargo tree -d` at S0; alt: rusqlite 0.40 + hand-rolled pool) · `sqlite-vec` 0.1.x (ffi 3-arg auto-ext) · `sonic-rs`+`serde_json` · `serde` · `memmap2` · `rayon` · `clap` 4 · `simsimd` · `bytemuck` · `etcetera` · `fs4` (file lock) · `time`/`chrono` · `indicatif` · `comfy-table` · `nu-ansi-term` · `thiserror`/`anyhow` · `tracing`/`tracing-subscriber` · dev: `insta`,`assert_cmd`,`predicates`,`trycmd`,`criterion`,`tempfile`. semantic(opt-in): `candle-core`/`candle-nn`/`candle-transformers` 0.10.x, `tokenizers`, `hf-hub` 0.5.0 (`default-features=false, features=["tokio","rustls-tls"]`). tui(opt-in): `nucleo`,`ratatui`. release profile: `opt-level="z"`,`lto="fat"`,`codegen-units=1`,`panic="abort"`,`strip=true`.

---

## Iteration log
- **Iteration 0** (2026-06-14): detailed plan from 5-agent research.
- **Iteration 3 — CONSENSUS** (3 agents + Codex): Schema/SQL **[CONSENSUS]**, Parsing/recovery **[CONSENSUS]** (real-data: 100% message coverage via traverse-all + bridge). Final fixes: **rusqlite/r2d2_sqlite version coupling** — `r2d2_sqlite 0.34` needs `rusqlite ^0.39` (`<0.40`), so pinned **rusqlite 0.39.x** (default) with a hand-rolled-pool alt on 0.40 (Codex 🔴); stray `rusqlite 0.38` in ARCHITECTURE §1 → 0.39; added `sessions_fts` sync triggers to DDL; fixed `directories`→`etcetera` citation. All dimensions consensus.
- **Iteration 2** (4 agents + Codex): fixed ⚠ `sessions_fts` now external-content over a real `sessions.title` column (was referencing a non-existent column → runtime error); **all other `system/*` must TRAVERSE in DFS** (turn_duration/stop_hook_summary/scheduled_task_fire/… parent real turns — stopping lost 27–72% of messages), not "skip"; **content location by type** (`attachment`→`obj.attachment.content`, `progress`→`obj.data`, not `message.content`); `isCompactSummary` is top-level not under `message`; **hook closes stdin** (`</dev/null`, #43123 hang after v2.1.87); `messages_au` UPDATE trigger guarded both sides; `workflows.parent_session_fk` ON DELETE CASCADE; `foreign_keys=ON` on every connection; `r2d2_sqlite 0.34`/verify single `libsqlite3-sys`; hf-hub `default-features=false`+tokio+rustls-tls; musl wording corrected (core also bundles C, needs build-time C cross-compiler, no runtime system lib). Cross-doc reconciled: **PLAN §105 fastembed→Candle**, ARCHITECTURE `default=[]` (was `["tui"]`), illustrative Cargo.toml rusqlite 0.40 + etcetera, int8→f32/130 MB, cargo-dist axodotdev.
- **Iteration 1** (5 agents + Codex): fixed ⚠ **forest roots = ALL parent_uuid==null + traverse all node types** (real-data: was losing 1471/1555 msgs); content in `message.content`; **always store rows, omit only empty FTS**; **subagent scope pinned** (topology-only by default, `--include-subagents` opt-in) + glob disambiguation; **idempotent per-file replacement** + ON DELETE CASCADE + explicit vec/relations cleanup + idempotency test; **chunk-level embeddings** (`chunks`+`vec_chunks`, was wrongly message-keyed); rusqlite real version + r2d2 match; sqlite-vec 3-arg signature; PRAGMA split (writer WAL, readers read-only, `pragma_update`, `busy_timeout`); `sessions_fts` external-content (snippet works); **FTS MATCH query compiler/escaping**; ordered parallel reduction; routing for attachment/progress; cross-file orphan fallback; topology cwd cross-validation; display-id model; hf-hub 0.5.0/`with_cache_dir`+rustls-tls; candle f32 + musl-onig C-toolchain caveat; cargo-dist upstream axodotdev; rcodesign notarize-not-staple; `/recall:search` as command not disable-model-invocation skill (#26251); default features lean (tui opt-in); split CI feature matrix; timestamp parsing; file lock + busy_timeout. ARCHITECTURE §3/§4 (int8→f32, 33MB→130MB) and §7 (astral→axodotdev) to be corrected.
