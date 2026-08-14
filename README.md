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

The default build makes no network calls at all. One opt-in feature can:
`--features azure` sends prompt text to an Azure OpenAI endpoint **you** own, and
only when you have set all three `WEND_AZURE_*` variables. See
[Semantic search](#semantic-search).

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

# optional: semantic (meaning-based) search — see below for the two backends
cargo install --path crates/wend-cli --features semantic
wend index --embed                     # downloads the e5 model once, embeds your prompts
```

## Semantic search

`wend search --semantic` fuses keyword (BM25) with vector similarity over your
own prompts. Two opt-in backends; the default build has neither.

| build | backend | where your text goes |
|---|---|---|
| *(default)* | none — keyword only | — |
| `--features semantic` | `multilingual-e5-small` via ONNX | stays on your machine |
| `--features azure` | Azure OpenAI `text-embedding-3-large` | your own Azure resource |

Measured on one real corpus (757 sessions, 20k chunks) with 34 natural-language
Turkish queries written to share no wording with the message they target — the
realistic case where you remember the topic, not the words. Two different
things were measured, and they are not interchangeable:

**Backend comparison** (rank 500 candidate messages, identical harness):

| backend | MRR@10 |
|---|---:|
| Azure `text-embedding-3-large` @1024 | 0.771 |
| local `multilingual-e5-small` @384 | 0.381 |
| keyword only | 0.000 |

**End to end** (`wend search --semantic` against the whole index, does the exact
session you meant come back): top-1 24%, top-5 50%, top-10 56%, MRR 0.350 — versus
**0.000 for keyword search**, which returns nothing at all for a natural-language
query because every term is ANDed together. The end-to-end number is lower
because picking one session out of 757 is much harder than picking one message
out of 500, and because many sessions are genuinely about the same topic.

Your numbers will differ. The harness is in `plans/`.

The Azure backend is inert until all three variables are set:

```bash
export WEND_AZURE_ENDPOINT=https://<your-account>.cognitiveservices.azure.com/
export WEND_AZURE_KEY=<key>
export WEND_AZURE_DEPLOYMENT=<your-embedding-deployment>
wend index --embed
```

Setting only some of them is an error rather than a silent fallback, because
falling back would overwrite every Azure vector in your index.

**What leaves your machine.** Only the prompts *you typed* (never tool output or
transcripts of Claude's replies), and only when you run `wend index --embed` or
`wend search --semantic`. Text is scanned for secrets first — AWS keys, GitHub /
GitLab / npm / Slack / Google / SendGrid tokens, `sk-`-style API keys, JWTs,
bearer tokens, `*_TOKEN`/`*_SECRET`/`*_API_KEY` assignments, private-key blocks
(including fragments split across chunks), and URIs with inline credentials —
and those are replaced before the request is built.

That is pattern matching, not a guarantee: it raises the floor, it cannot
promise nothing sensitive ever escapes. On the corpus it was developed against
it caught 29 of 29 detectable secrets while altering 0.3% of ordinary prompts,
but your history is not that history. Cost is roughly $0.13 per million tokens;
a 20k-chunk history is about $0.90 once and ~18 minutes, then pennies
incrementally.

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
