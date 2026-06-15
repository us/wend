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

```bash
# from this repo
cargo install --path crates/recall-cli   # puts `recall` on your PATH
recall index                             # build the index (~15s for ~200 sessions, then incremental)
recall doctor                            # check status

# optional: semantic (meaning-based) search — heavier build, pure-Rust Candle model
cargo install --path crates/recall-cli --features semantic
recall index --embed                     # downloads the bge model once, embeds sessions
```

## Use

```bash
recall search "rust sqlite fts"           # keyword (BM25, stemmed, session-grouped)
recall search "fixing a crash" --semantic # meaning-based (hybrid keyword+vector); needs --features semantic build
recall search "auth bug" --json           # machine-readable (for scripts/skills)
recall show <id>                          # read a transcript (--head/--tail to window)
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
(+recovered), resume, name, tree, doctor — verified on a real 213-session /
178k-message corpus. Semantic search is opt-in (`--features semantic`): pure-Rust
Candle `bge-small-en-v1.5`, one vector per session, cosine + RRF fusion with
keyword (first `index --embed` downloads the model and is slow on CPU, then
incremental). Not yet implemented: subagent indexing (`--include-subagents`),
`export`, cross-platform release binaries. See `PLAN.md` / `ARCHITECTURE.md` /
`IMPLEMENTATION.md`.

MIT.
