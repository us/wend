# Claude Code Session History Tool — Implementation Plan (v3)

A local-first, open-source Claude Code **skill + indexer CLI** to find, identify, retrieve, **recover (pre-compaction)**, and visualize the **relationship topology** of a developer's many past Claude Code sessions — from inside the terminal.

> **Status: consensus reached (plan-loop, 3 iterations).** v1 premise (empty wedge) was false; v2 re-grounded on validation-first + topology; v3 fixed verified data-count errors, made the recovery/topology **data model** sufficient, added a **security** section. **Language committed: Rust** (single static binary — lightness/speed + zero packaging risk). **Name chosen: `wend`** (the earlier `recall` was dropped — 6+ collisions and a "memory" semantic clash; `wend` is free on crates.io/npm/GitHub with no similar tool).

---

## Context

### Problem
A heavy Claude Code user accumulates an unsearchable history. ⚠ **Corrected scale (iteration-2/3 verified):** `~/.claude/projects/` holds **~206 top-level session `.jsonl` files (~0.74 GB)** plus **~6,040 subagent `.jsonl` files** nested under `<sessionId>/subagents/...` (total ~6,246 files, ~1.6 GB). The indexer must treat **top-level sessions** and **subagent transcripts** as distinct corpora with **distinct globs** — earlier "6,152 sessions" conflated them. Pains: (1) find/retrieve by NL; (2) **compaction blindness** (pre-summary turns hidden); (3) **worktree/subagent topology opacity**.

### Competitive reality (verified — the load-bearing section)
Crowded, fast-moving, NOT empty:
- **FlineDev/Recall** — already does post-compaction recovery via hooks (~15–18K tokens). Our supposed wedge, shipped.
- **samzong/Recall** — local-first hybrid FTS+semantic, multi-tool, JSONL export/import across machines (the "paid sync," free), Homebrew. Whole roadmap, minus topology.
- **cc-conversation-search** — a Claude Code **skill**+CLI; markets semantic search, has `--json` export and a `tree` command — ⚠ verified: that `tree` shows the **within-session message tree**, NOT git-worktree/subagent topology. The topology gap is real.
- **claude-history** (Rust) — hybrid TUI; single-binary install with **no interpreter dependency**.
- **Native rewind/checkpointing** — ⚠ (Codex correction) phrase precisely: "native rewind/checkpointing **exists and is evolving**" (double-Esc rewind menu + VS Code checkpointing; not a documented stable `/rewind` command). Strategic-obsolescence risk stands.
- 6+ tools named "Recall" + dense memory-tool field → name is a commodity, organic discovery ≈ 0.

**Conclusion:** no category-creation play. Entry = out-execute-on-quality + the one under-served surface: **worktree↔subagent↔session relationship topology**. ⚠ But that surface carries the **same** obsolescence risk as recovery — Claude Code is actively building first-class worktree/subagent tooling (v2.1.50). **Phase -1's go/no-go must explicitly test: "is Anthropic likely to ship native topology within ~2 quarters?"** Everything else (search/recovery/export) is table-stakes we match.

### Verified format facts (filesystem-checked; ⚠ = corrected this iteration)
- **Path**: `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`; `sessionId` == stem. Subagents: `<sessionId>/subagents/agent-<hex>.jsonl` (+`.meta.json`) and `<sessionId>/subagents/workflows/wf_<id>/{agent-*.jsonl, journal.jsonl}`; `<sessionId>/workflows/` holds `wf_<id>.json` (single JSON, not jsonl) + `scripts/`; there is also a `<sessionId>/tool-results/` dir. ⚠ `journal.jsonl` (carries `started`/`result`) lives **only** under `subagents/workflows/wf_<id>/`, never directly in `workflows/`.
- **Dir encoding** (lossy): `/`→`-` **and** `.`→`-` (both become dash; an adjacent `/.` → `--`), e.g. `/.claude/`→`--claude-`. ⚠ The dot is NOT dropped — it becomes a dash; this matters for Phase 4's `originalCwd`→encoded-dir reverse resolver. Authoritative project path = per-line **`cwd`**, present on `user`/`assistant`/`system`/`attachment`/`progress` lines; absent on `ai-title`/`last-prompt`/`permission-mode`/`mode`/`file-history-snapshot`/`pr-link`/`worktree-state`/`queue-operation`/`agent-name`. Cache cwd from first conversation line.
- **compact_boundary**: `system` line, `subtype:"compact_boundary"`, `parentUuid:null`, `logicalParentUuid`→pre-compaction tail, `compactMetadata:{trigger,preTokens,postTokens}`. Present 100% of boundaries. ⚠ **There is NO `type:"summary"` line** — the reliable summary artifact is a **`user` line with `isCompactSummary:true`**, present in **100% of compacted sessions** (v2's "~2–3%" was wrong). `ai-title` is the title source (take latest; ⚠ count can be **high — up to ~829/session**, and format switches human-readable→slug after compaction); `custom-title` is user-set.
- ⚠ **Cross-file recovery** is **rare (~0.3%, 1/311 on this corpus)** but real (bg/resumed sessions) → recovery falls back to sibling-file search. "Pre-compaction messages stay in the same file" holds ~99.7%.
- **Forest, not chain**: multiple `parentUuid:null` roots per file → order roots by `timestamp`, stitch all chains.
- **worktree-state** lives in the worktree's own file: `worktreeSession:{originalCwd, worktreePath, worktreeName, worktreeBranch, originalBranch, originalHeadCommit, sessionId}`. ⚠ inner `sessionId` = the session this worktree **continues** (self or prior sibling) — store it as `continues_session_id`. Parent link to original repo exists **only via `originalCwd`** and needs a **deterministic resolver** (below), not bare path-matching.
- ⚠ **`.meta.json` has 5 shapes** (~counts): `{agentType}` (~4,055) · `{agentType,description}` (~1,402) · `{agentType,description,toolUseId}` (~558) · `{agentType,description,name}` (~14) · `{agentType,description,name,toolUseId}` (~2). `agentType` is always present; null-guard **both** `toolUseId` and `name`.
- **Content blocks** (~9 distinct): top-level string | `text` | `thinking` | `tool_use` | `tool_result` | `image`; `tool_result.content` may be a list of `text`/`image`/`tool_reference`. ⚠ base64 `image` lines reach **~3 MB** (not 588 KB) — stream/skip, never load into FTS. Exclude `image` + default-exclude `thinking` from FTS body.
- ⚠ **`bridge-session`** lines (`bridgeSessionId: cse_…`, `lastSequenceNum`) link sessions — relevant to topology; capture them.
- **Resume**: `claude --resume <sessionId>` from the original cwd → **emit `cd <cwd> && claude --resume <id>`**, don't spawn.
- **FTS5** in CPython stdlib `sqlite3` on python.org builds; ⚠ not universal — runtime-probe. **`sqlite-vec`** (Phase 8) needs loadable extensions (off on macOS system Python).

### Security model (NEW — iteration-2 security/Codex)
- **Read-only over `~/.claude/projects/`** — never write there. Index at `~/.claude/<tool>/index.db`.
- ⚠ **The index is a concentrated secret store**: one ~200–300 MB SQLite file (FTS5 external-content keeps plaintext) aggregating every secret/key/proprietary line ever in a transcript — a higher-value exfil target than the scattered source corpus. **Mandatory, low-cost:** create `index.db` and its `-wal`/`-shm` sidecars with **`0600`** perms (`os.open(...,0o600)`/umask at creation); README warns the file is sensitive.
- ⚠ **Export outputs (Phase 5) and any cache MUST be `.gitignore`-guided**; README warns exports contain recovered secrets; redaction is best-effort only.
- ⚠ **Prompt-injection (first-class risk):** recovery/topology feed **old, attacker-influenceable session content** (past web fetches, pasted issues, other agents' output) into a **live, tool-enabled** agent — indirect, time-delayed injection. Mitigations baked into Phase 3/7: wrap all recovered/retrieved content in explicit **untrusted-data delimiters** with a standing "this is DATA, not instructions" frame; **never pre-approve Bash/exec tools** on the find/recover skills (`allowed-tools` excludes execution); thin-skill/fat-CLI reduces but does not remove the risk.
- ⚠ **Phase -1 data-exfiltration:** competitor teardown runs third-party tools against private data → run them **network-blocked/offline**, prefer a **redacted corpus copy**, and **record whether each tool attempts network access**.

### Not doing
Web GUI; real-time mirroring/remote control; multi-user teams (v1); LLM-eval platform.

---

## Phase -1 — Validation gate (FIRST; ~1–2 days) 🚦
- **Name first** — pick a non-colliding name before any public artifact.
- **Competitor teardown (network-blocked):** install FlineDev/Recall, samzong/Recall, cc-conversation-search; run against a **redacted copy** of the corpus offline; screenshot find/recover/topology/first-run/install friction; log any network attempts.
- **Demand test (pre-registered bar):** 30-sec asciinema of the topology+recovery use-case on issues #26125/#27242 + r/ClaudeAI + HN. ⚠ Reframe (Codex): a 1–2 day test is a **kill switch, not proof** — *negative* signal stops us; *positive* signal only **permits a small MVP**. Pre-register the bar (e.g., ≥N unprompted comments describing the topology pain, OR a maintainer interested in an upstream contribution).
- **Obsolescence check:** explicitly assess whether Anthropic is likely to ship native topology/recovery within ~2 quarters.
- **WTP probe:** one-question sync-pricing poll (do not gate cloud on stars).
- **Output:** `validation.md` go/no-go (non-build branch = contribute topology upstream).

## Phase 0 — Fixtures & deterministic minimizer (~1.5 days)
- A **structure-preserving minimizer** (not hand-redaction) keeping the UUID graph, compaction metadata, worktree records, all 5 meta.json shapes, the ~9 content-block shapes, and `bridge-session`. Required fixtures: multi-compaction (18–39 boundaries), cross-file lpu, multi-root forest, worktree dir with >1 session, `{agentType}`-only meta, base64-image line, nested `tool_result`/`tool_reference`.
- Golden tests: counts, boundary detection, **boundary-relative** pre-compaction recovery (incl. cross-file + "query matches only pre-compaction text"), forest stitching, worktree linkage + confidence, content flattening, **FTS consistency after file delete/modify/truncate/corrupt-line** (no orphan FTS hits).

## Phase 1 — Indexer core (~4–5 days; ⚠ up from 3) (free OSS)
- ⚠ **Language = Rust** (iteration-3 user decision: lightness + speed are first-class requirements, and it kills the entire packaging-risk class — see Phase 7-lite). Stack: `serde_json` (+`simd-json` if needed) for JSONL parse, `rusqlite` with the **`bundled`** feature (ships SQLite **with FTS5 compiled in** — no system-sqlite dependency, no interpreter/FTS5 probe), `clap` (CLI), `rayon` (parallel file parsing for the cold index). Edition 2021, `cargo clippy`/`fmt`. **WAL** + application-level single-writer **lock** (distinct from SQLite's WAL serialization — guards two sessions starting at once). `schema_version` + hand-written `IF NOT EXISTS`/`ALTER` migrations.
- **Schema** (⚠ expanded for provenance/recovery/topology — Codex):
  - `session_files(path PK, source_kind, head_tail_hash, mtime, size, parser_version, scan_started_at, scan_finished_at)` — incremental + bug-report provenance.
  - `sessions(id PK, session_id, source_kind, file_path, project_path, project_name, git_branch, first_ts, last_ts, ai_title, custom_title, message_count, has_compaction, indexed_at, UNIQUE(source_kind,session_id,file_path))`.
  - `messages(id PK, session_fk, uuid, parent_uuid, line_no, source_file, type, subtype, role, ts, cwd, content_json, text_for_fts, is_sidechain, is_compact_summary)` — ⚠ keep **normalized content JSON + line_no/source_file** so `show --recovered`/`export` render the **original** transcript, not a search digest. Index `(session_fk, uuid)` + global uuid lookup.
  - `boundaries(id PK, session_fk, uuid, parent_uuid, logical_parent_uuid, logical_parent_file, trigger, pre_tokens, post_tokens, ts)`.
  - `boundary_messages(boundary_fk, message_fk, path_order, distance, source)` — ⚠ pre-compaction is **boundary-relative**, not one boolean.
  - `relations(parent_fk, child_fk, relation_type[worktree|subagent|workflow|bridge|continues], evidence, confidence, source_path, tool_use_id, workflow_id)` — ⚠ topology as a **first-class** graph.
  - `workflows(id, parent_session_fk, wf_id, meta_path)` + `workflow_events(workflow_fk, kind[started|result], ts, payload)`.
  - `worktrees(session_fk, original_cwd, worktree_path, worktree_name, branch, original_branch, original_head, continues_session_id, link_confidence)`.
  - `messages_fts` (FTS5 external-content over `text_for_fts`, porter unicode61, optional trigram) + separate `sessions_fts` (title/alias); merge-rank both. Exact rowid/delete/update discipline; reindex/truncate/corrupt-line must leave no orphan FTS rows (tested in Phase 0).
- **Parsing:** distinct globs for top-level vs subagent vs workflow files; route by `type`/`subtype`; flatten per the 9-shape map (drop image/thinking from FTS, recurse `tool_result.content`); cache cwd; populate boundary/relation/workflow/worktree tables.
- **Robustness:** atomic per-file txn; tolerate trailing/corrupt lines; remove rows for deleted files; handle renamed dirs/truncation.
- **Realistic numbers** (⚠ corrected; Rust should beat these): cold index of **~206 top-level sessions (~0.74 GB)** — Rust + `rayon` parse target **< ~5 s** (Python baseline was ~10–25 s); index ≈ **200–300 MB** (base64 images excluded, so index size tracks text not raw GB); incremental ≈ instant. Verification records cold/warm/incremental separately, with progress UI.

## Phase 2 — Search + retrieve (~1.5 days)
- `search` (bm25 weighted title≫body × recency-decay × active-project; `--json`; merge sessions_fts+messages_fts), `show <id> [--recovered] [--range/--head/--tail]`, `resume <id>` (verify cwd exists; disambiguate; on missing dir return status+alternatives), `doctor`/`status` (paths, last-indexed, semantic/cloud/network state). ⚠ Define a **public display-id** model (internal PK vs Claude `session_id` vs file path; disambiguation rules for show/resume/export/name).

## Phase 3 — Compaction recovery (~2 days)
- Walk `parent_uuid` over the forest; bridge each boundary via `logical_parent_uuid` (cross-file fallback); reconstruct from `boundary_messages` + normalized `content_json` → **original** transcript, all boundaries. One command: `show --recovered`.
- ⚠ **In-session output cap (pinned):** default = write full transcript to file + return **path + bounded boundary-header** (cap header to N≈10 boundaries, paginate beyond); inline view hard-capped at **~50 KB / ~800 lines** with `--range`/`--head`/`--tail`; never dump multi-MB into live context. Wrap returned content in untrusted-data delimiters (security).

## Phase 4 — Worktree/subagent topology ⭐ (~2.5 days; ⚠ up from 2) (lead differentiator)
- `tree [project]`: build from `relations` + `worktrees` + `workflows`. ⚠ **Deterministic worktree-parent resolver** (Codex): rank candidates in the `originalCwd` repo by (a) explicit `continues_session_id`, (b) branch/head match, (c) nearest preceding activity in a time window; label every edge "explicit" vs "inferred" with confidence; handle **ghost dirs** (session subdirs with no `.jsonl`). Solve the `originalCwd`→encoded-dir mapping (lossy-encoding reverse) here.

## Phase 5 — Export (~1 day, fast-follow)
- `export <id> --format md|html|json`, honors recovery, renders from `content_json`. Redaction **best-effort, opt-in, labeled, preview/diff**; `.gitignore` + secret warnings.

## Phase 6 — Custom naming (folded into Phase 1/2; ~2–3 h)
- `name <id> "<alias>"` → `custom_title`, boosted in `sessions_fts`.

## Phase 7-lite — Distribution proof (⚠ first proof = MANUAL CLI install only — Codex)
- ⚠ **Language/packaging DECISION (committed, iteration 3): Rust single static binary.** This eliminates the entire Python packaging-risk class that dominated earlier drafts: no interpreter dependency, no "is FTS5 in this sqlite build?" probe (`rusqlite` `bundled` compiles SQLite+FTS5 in), trivial cross-compile. `cargo build --release` → one binary per target (macOS arm64/x64, Linux x64). Still verify on a clean machine with nothing installed. macOS signing/notarization (Gatekeeper) remains an explicit task; binary ~5–15 MB. (Python+PyInstaller is the abandoned alternative; `shiv`/`pex` were rejected for needing a system Python.)
- **First proof ships via `cargo install` / `brew` / a direct binary download + manual CLI use**, not a plugin — get user feedback fastest. Plugin packaging (skill + hooks + bundled binary) is a *later* phase once the thesis holds.

## Phase 7 — Skill + plugin packaging (later) (free OSS)
- ⚠ **Two skills:** (a) always-on NL skill, tight **directive mostly-English** `description` (≤1,536-char fixed truncation on `description`+`when_to_use` in the skill listing — not a configurable knob; 1–2 foreign phrases max, rest in `when_to_use`; avoid generic "nerede konuştuk" → over-fire); (b) explicit `/<tool>:find`. `disable-model-invocation:true` removes the description from context (kills auto-fire) → can't be the same skill.
- ⚠ **Auto-index = detached-only.** `SessionEnd` is a real event but **too constrained/unreliable** (1.5 s default, ≤60 s, killed mid-run #41577 → partial-DB risk). Use `SessionStart`/`UserPromptSubmit` for a **cheap stat-only** check that **spawns a detached `nohup … & disown` writer** (never block startup — `SessionStart` blocks the session until the hook returns; async does not apply to a blocking event); guard with the WAL lock; **hooks must be silent on stdout** (stdout is injected into context). Reader tolerates a stale index ("indexing in progress").
- Thin skill / fat CLI (locked). First-run progress UX. `allowed-tools` excludes exec (injection). Own `marketplace.json`.
- ⚠ **CI/release acceptance matrix:** `cargo build --release` for macOS arm64/x64, Linux x64, (Windows if supported); clean-machine smoke test; `claude plugin validate`, hook dry-run, uninstall cleanup.

## Phase 8 — Local semantic (~3 days, fast-follow)
- Opt-in `--semantic`: **Candle (pure-Rust) + bge-small-en-v1.5 (f32, CLS pooling)** + `sqlite-vec` (static, via `rusqlite`); chunk-level vectors (~500-tok/15% overlap); RRF (k≈60); model downloaded on first run; explicit network-consent. (⚠ ONNX/`fastembed-rs` was rejected — breaks musl/portability; see ARCHITECTURE §3 and IMPLEMENTATION §7.)

## Phase 9 — Hosted cloud sync (PAID) — PARKED, unvalidated
- Sync is already free elsewhere (samzong) + DIY (git/syncthing); realistic conversion <1–2%; OSS user least monetizable. ⚠ Reframe: **encrypted content sync + uploaded client-side embeddings, NOT zero-knowledge semantic** (embedding-inversion leaks). Key mgmt unsolved (lost-key = lost-data). Project identity = ranked signals (git remote URL → root commit → repo name → normalized cwd). **The only plausible non-OSS buyer is team session-audit/compliance — that's a pivot, not a feature.** Do NOT build until a real WTP signal from that segment.

---

## MVP scope & monetization
- **Validation MVP:** Phase -1 → 0 → 1 → 2 → 3 → 4 → **7-lite (manual CLI)**. ⚠ **Effort ≈ 3 weeks** (Codex; v2's 12.5–13 d optimistic). Thesis test = **do users engage with the topology/recovery view** (not "out-polish a year-old repo on day one").
- **Fast-follow:** 5 (export), 6 (naming), 7 (plugin), 8 (semantic). **Parked:** 9 (cloud).
- **Monetization (honest):** moat is thin and the plan says so — search/recovery/export are table stakes; only topology + UX differentiate, both copyable; native features encroaching. **Treat as an OSS/portfolio/reputation play first**; gate any paid work on a real WTP signal from a non-OSS (team-audit) buyer.

## Verification (additions)
Phase -1: `validation.md` + network-access log per competitor tool. Phase 0: golden + FTS-consistency-after-mutation tests. Phase 1: cold/warm/incremental timings separate; 0600 perms on db+wal+shm asserted; deleted-file rows removed; lock holds under concurrent start. Phase 2: known-item top-3; missing-cwd reported; display-id disambiguation. Phase 3: recovered count == raw survivors incl. cross-file; pre-compaction is boundary-relative; inline cap (≤50 KB) enforced; untrusted-data wrapping present. Phase 4: tree correct + confidence labels + ghost dirs; resolver deterministic. Phase 5: round-trip; HTML offline; redaction labeled + preview. Phase 7-lite: `cargo build --release` binary runs on a clean machine (nothing installed); FTS5 available via `rusqlite bundled`. Phase 7: per-skill 1,536 budget (`/doctor`); fresh-session trigger (fires on intended, not generic); hooks silent on stdout; auto-index never leaves partial DB. Phase 8: hybrid beats keyword on labeled set; graceful degrade.
Commands: `cargo test`, `cargo clippy`, `cargo build --release`, `<tool> index|search|show --recovered|tree|doctor`, `claude plugin validate`.

## Open questions (reduced)
1. Go/no-go after Phase -1 (incl. the topology-obsolescence assessment).
2. ~~Language~~ — RESOLVED: **Rust** (single static binary; `rusqlite bundled` for FTS5). Remaining sub-task: confirm macOS notarization/signing flow in CI.
3. Final name (before public demo).
4. How much of the old corpus lacks `worktree-state` (→ how much the tree leans on low-confidence inference).

## Order & risk
| Phase | Effort | Risk | Order |
|---|---|---|---|
| -1 Validation (+name, security-safe teardown) | 1–2d | de-risks | 1 |
| 0 Fixtures + minimizer | 1.5d | Low | 2 |
| 1 Indexer core (provenance/graph schema) | 4–5d | High | 3 |
| 2 Search + retrieve + doctor | 1.5d | Low-Med | 4 |
| 3 Compaction recovery | 2d | Med-High | 5 |
| 4 Worktree/subagent topology ⭐ | 2.5d | Med-High | 6 |
| 7-lite Distribution proof (manual CLI, Rust release binary) | 2–3d | Med | 7 |
| 5 Export | 1d | Low | fast-follow |
| 6 Naming | 0.3d | Low | folded |
| 7 Plugin packaging | 3d | High | fast-follow |
| 8 Local semantic | 3d | Med | fast-follow |
| 9 Cloud (paid) | — | High | PARKED |
Validation MVP ≈ **3 weeks**, gated on Phase -1 go.

---

## Iteration log
- **Iteration 0** (2026-06-13): initial plan, 5-agent research.
- **Iteration 1**: re-grounded strategy (wedge occupied: FlineDev/samzong/cc-conversation-search + native rewind), added Phase -1, repositioned on topology, fixed format facts, replaced SessionEnd auto-index, fixed bin/PATH, added graph tables.
- **Post-consensus** (2026-06-13): moved plan to `~/coding/wend/PLAN.md`. **Language decision changed Python→Rust** at user request (lightness + speed are first-class). This also retires the plan's single biggest residual risk: the Python packaging/interpreter/FTS5-probe problem disappears (Rust single static binary, `rusqlite bundled` ships FTS5). Phase 7-lite risk High→Med; cold-index target tightened to <~5 s. Open question #2 resolved.
- **Iteration 3** (3 reviewers + Codex): Strategy **[CONSENSUS]**, Security/packaging/hooks **[CONSENSUS]**, Codex **[CONSENSUS]**. Data-model/format raised the last 🔴 + 2 🟡, all fixed in-place: dot-encoding rule corrected (`.`→`-`, NOT dropped — the `--claude-` example was right, the prose was wrong); top-level corpus size **~0.74 GB / ~206 sessions** (was ~0.32 GB), total ~1.6 GB; meta.json counts trued-up; dropped the non-existent `maxSkillDescriptionChars` knob; `async` wording softened. **Consensus reached.**
- **Iteration 2** (5 reviewers + Codex): ⚠ fixed verified data-count errors (**~205 top-level sessions/~0.32 GB**, not 6,152/1.5 GB; subagents separate); **no `type:"summary"`** → use `isCompactSummary` (100%); corrected subagent layout (`journal.jsonl` under `subagents/workflows/wf_*/`; `tool-results/`); meta.json 5 shapes + `name` null-guard; ai-title max ~829; base64 ~3 MB; cross-file lpu ~0.3%; `cwd` on `progress`; `bridge-session` for topology. **Data model**: added `content_json/line_no/source_file`, `boundary_messages` (boundary-relative), first-class `relations`+`workflows`+`workflow_events`, `worktrees.continues_session_id`, `session_files` provenance, public display-id. **Security**: index.db 0600 + sidecars + .gitignore/export warnings; **prompt-injection** untrusted-data framing + no pre-approved exec; Phase -1 network-blocked teardown. **Packaging committed**: PyInstaller `--onefile` (shiv/pex rejected), Rust fallback. **Hooks**: SessionEnd "too constrained" (not "not real"), detached-only writer, silent stdout. **Strategy**: native-rewind phrasing softened, topology-obsolescence added to gate, demand test = kill-switch w/ pre-registered bar, name-before-demo, thesis≠out-polish, effort → ~3 weeks, first proof = manual CLI not plugin. Conflicts resolved with deeper filesystem evidence. No findings rejected outright.
