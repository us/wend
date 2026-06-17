# session-recall (`recall`)

Find, recover, and resume your past **Claude Code** sessions — from the terminal
or from inside Claude Code itself. Fast, local, single binary, zero network.

You run dozens of Claude Code sessions across many directories and can't find the
one you need. `recall` indexes your whole local history (`~/.claude/projects`) and
lets you:

- **find** a past session by keyword — `recall search "that firecrawl pricing chat"`
- **recover** the pre-compaction history the live UI hides — `recall show <id> --recovered` ⭐
- **resume** an old session — `recall resume <id>` → `cd … && claude --resume …`
- **read / label / map** — `recall show`, `recall name <id> "<alias>"`, `recall tree`

Everything is local and read-only over your transcripts; the only state it writes
is its own index (`~/.local/share/recall/index.db`, `0600`).

## Install

**Prebuilt binary** (once a `v*` tag is released): download the archive for your
platform from GitHub Releases (macOS arm64/x64, Linux x64 gnu+musl static,
Windows x64), unpack, and put `recall` on your PATH. These are the lean
keyword-only build. On macOS a downloaded binary is Gatekeeper-quarantined unless
notarized; `brew`/`curl|sh` installs avoid the quarantine bit.

**From source:**

```bash
cargo install --path crates/recall-cli   # puts `recall` on your PATH
recall index                             # build the index (~15s for ~200 sessions, then incremental)
recall doctor                            # check status

# optional: semantic (meaning-based) search — heavier build (ONNX Runtime)
cargo install --path crates/recall-cli --features semantic
recall index --embed                     # downloads the e5 model once, embeds your prompts
```

## Use

```bash
recall search "rust sqlite fts"           # keyword (BM25, stemmed, session-grouped)
recall search "fixing a crash" --semantic # meaning-based (hybrid keyword+vector); needs --features semantic build
recall search "auth bug" --json           # machine-readable (for scripts/skills)
recall show <id>                          # read a transcript (numbered messages, total shown)
recall show <id> --count                  # just how many messages
recall show <id> --range 10:20            # messages 10–20 (also --head N / --tail N)
recall show <id> --recovered              # surface pre-compaction history the UI hid
recall resume <id>                        # prints the cd + claude --resume command
recall name <id> "payment-spike"          # alias a session so you can find it later
recall tree [project]                     # worktree/session topology
```
Short session-id prefixes work everywhere (`recall show f8bd399d`); ambiguous
prefixes list the candidates.

## Use it from inside Claude Code (plugin)

```bash
claude plugin marketplace add ~/coding/session-recall
claude plugin install recall@recall
```
This adds:
- a **`find`** skill that auto-fires on natural language ("where was that chat
  about X?", "recover the compacted history", "nerede konuşmuştuk") and calls `recall`,
- a **`/recall:search`** command,
- a **SessionStart hook** that keeps the index fresh in the background.

(The plugin calls the `recall` binary, so install it on your PATH first.)

## Status

Working today: index, search (keyword + optional **semantic** hybrid), show
(+recovered, --range/--count, numbered messages), resume, name, tree, doctor —
verified on a real 213-session / 178k-message corpus. Semantic search is opt-in
(`--features semantic`): `fastembed` (ONNX Runtime) with the multilingual
`multilingual-e5-small` model, embedded at the **chunk** level over your own
prompts, cosine + RRF fusion with keyword (first `index --embed` downloads the
model once, then incremental; thread use is capped so it won't pin the machine —
override with `RECALL_EMBED_THREADS`). Not yet implemented: subagent indexing
(`--include-subagents`), `export`. See `PLAN.md` / `ARCHITECTURE.md` /
`IMPLEMENTATION.md`.

MIT.
