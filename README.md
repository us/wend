# wend

Find, recover, and resume your past **Claude Code** sessions — from the terminal
or from inside Claude Code itself. Fast, local, single binary, zero network.

You run dozens of Claude Code sessions across many directories and can't find the
one you need. `wend` indexes your whole local history (`~/.claude/projects`) and
lets you:

- **find** a past session by keyword — `wend search "that firecrawl pricing chat"`
- **recover** the pre-compaction history the live UI hides — `wend show <id> --recovered` ⭐
- **resume** an old session — `wend resume <id>` → `cd … && claude --resume …`
- **read / label / map** — `wend show`, `wend name <id> "<alias>"`, `wend tree`

Everything is local and read-only over your transcripts; the only state it writes
is its own index (`~/.local/share/wend/index.db`, `0600`).

## Install

**One-liner** (prebuilt binary, macOS + Linux):

```bash
curl -fsSL https://raw.githubusercontent.com/us/wend/main/install.sh | sh
```

Pin a version with `WEND_VERSION=v0.1.0` or change the target dir with
`WEND_INSTALL_DIR=~/.local/bin`. On macOS, `curl|sh` installs avoid the
Gatekeeper quarantine bit (a binary downloaded via the browser would be
quarantined unless notarized).

**Prebuilt binary (manual):** download the archive for your platform from
[GitHub Releases](https://github.com/us/wend/releases) (macOS arm64/x64,
Linux x64 gnu + musl static, Windows x64), unpack, and put `wend` on your PATH.
These are the lean keyword-only build.

**From source:**

```bash
cargo install --path crates/wend-cli   # puts `wend` on your PATH
wend index                             # build the index (~15s for ~200 sessions, then incremental)
wend doctor                            # check status

# optional: semantic (meaning-based) search — heavier build (ONNX Runtime)
cargo install --path crates/wend-cli --features semantic
wend index --embed                     # downloads the e5 model once, embeds your prompts
```

## Use

```bash
wend search "rust sqlite fts"           # keyword (BM25, stemmed, session-grouped)
wend search "fixing a crash" --semantic # meaning-based (hybrid keyword+vector); needs --features semantic build
wend search "auth bug" --json           # machine-readable (for scripts/skills)
wend show <id>                          # read a transcript (numbered messages, total shown)
wend show <id> --count                  # just how many messages
wend show <id> --range 10:20            # messages 10–20 (also --head N / --tail N)
wend show <id> --recovered              # surface pre-compaction history the UI hid
wend resume <id>                        # prints the cd + claude --resume command
wend name <id> "payment-spike"          # alias a session so you can find it later
wend tree [project]                     # worktree/session topology
```
Short session-id prefixes work everywhere (`wend show f8bd399d`); ambiguous
prefixes list the candidates.

## Use it from inside Claude Code (plugin)

```bash
claude plugin marketplace add ~/coding/wend
claude plugin install wend@wend
```
This adds:
- a **`find`** skill that auto-fires on natural language ("where was that chat
  about X?", "recover the compacted history", "nerede konuşmuştuk") and calls `wend`,
- a **`/wend:search`** command,
- a **SessionStart hook** that keeps the index fresh in the background.

(The plugin calls the `wend` binary, so install it on your PATH first.)

## Status

Working today: index, search (keyword + optional **semantic** hybrid), show
(+recovered, --range/--count, numbered messages), resume, name, tree, doctor —
verified on a real 213-session / 178k-message corpus. Semantic search is opt-in
(`--features semantic`): `fastembed` (ONNX Runtime) with the multilingual
`multilingual-e5-small` model, embedded at the **chunk** level over your own
prompts, cosine + RRF fusion with keyword (first `index --embed` downloads the
model once, then incremental; thread use is capped so it won't pin the machine —
override with `WEND_EMBED_THREADS`). Not yet implemented: subagent indexing
(`--include-subagents`), `export`. See `PLAN.md` / `ARCHITECTURE.md` /
`IMPLEMENTATION.md`.

MIT.
