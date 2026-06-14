# Architecture & Tech Stack

Companion to `PLAN.md`. This document fixes the concrete technical decisions, optimizing — in priority order — for **speed**, **quality**, **ease-of-use**, and the hard requirement: **runs on every computer with zero setup** (single static binary, no Python, no GPU, no system libraries, no network for core features).

Language: **Rust** (edition 2021). Everything below is grounded in iteration-3 parallel research (5 agents, web-sourced); citations at the end of each section.

---

## 0. Guiding constraints (what every decision is measured against)
1. **Zero-setup portability** — one self-contained binary that works on macOS (arm64+x64), Linux (glibc **and** musl), Windows. No runtime shared libs, no "install X first." This single constraint kills several otherwise-attractive options (see §3).
2. **Speed** — cold keyword index of ~0.74 GB in **a few seconds**; any search **< ~50 ms**; CLI startup in **ms**.
3. **Quality** — BM25 + semantic hybrid ranking; recover full pre-compaction history faithfully.
4. **Ease-of-use** — `brew install` / `curl | sh`, sensible defaults, instant interactive picker, great first-run UX, and an in-Claude-Code skill.
5. **Privacy** — transcripts contain code/secrets; nothing leaves the machine for any core feature. Cloud is opt-in and later (parked).

---

## 1. Storage & keyword search — **rusqlite (bundled) + SQLite FTS5**

**Decision:** one SQLite database file holding metadata, normalized message content, the graph tables (boundaries/relations/etc. from `PLAN.md`), the FTS5 index, AND (later) vectors. `rusqlite = { version = "0.39", features = ["bundled"] }` (pinned to match `r2d2_sqlite 0.34`; see IMPLEMENTATION §0/§12) compiles SQLite **with FTS5** straight into the binary — no system SQLite, no setup, works on every target.

- **Search:** FTS5 `bm25()` ranking + `snippet()`/`highlight()` for excerpts. Tokenizers: `porter unicode61` (stemming) for the body; an optional `trigram` table for substring/typo-ish matching. External-content table over `messages` (no text duplication) — must issue the `'delete'` command with old values on update/delete to keep the index from drifting (handled in the indexer's per-file atomic txn).
- **Why not Tantivy:** pure-Rust, genuinely faster (sub-ms) and has real fuzzy (`FuzzyTermQuery`), but it's a separate multi-file index dir — two storage formats to maintain and ship. At ~300k docs FTS5 queries are single-digit ms (fast *enough*), and one portable `.db` file wins decisively on ease-of-use/zero-setup. **Revisit Tantivy only if** typo-tolerant ranking becomes a hard requirement.
- **musl gotcha:** always build with `bundled` (never link system sqlite); the old musl segfault (#914) is resolved in modern versions but test the musl target in CI.

Sources: rusqlite bundled PR #176 · sqlite.org/fts5 · quickwit-oss/tantivy.

## 2. Two-tier search model (this is the UX/perf backbone)

The product ships **two search tiers** so the common case is instant and the heavy case is opt-in:

- **Tier 1 — Keyword (default, always on):** FTS5/BM25. Cold index **2–4 s**, query **< 10 ms**. Zero model, zero network, works the instant you install. This alone covers ~80% of "where was that chat about X."
- **Tier 2 — Semantic (opt-in):** local embeddings + vector search, fused with Tier 1 via RRF. Enabled with `--semantic` / `recall index --embed`. The embedding build is the only slow step (see §3) so it runs as an explicit, resumable, incremental background pass — never blocking Tier 1.

This split means ease-of-use is never gated on the slow path, and "runs everywhere" holds even if a user never enables semantics.

## 3. Local embeddings — **Candle (pure-Rust) + bge-small-en-v1.5**, model fetched on first run

**This is the decision the "runs on every computer" requirement forces.**

- ❌ **fastembed-rs / `ort` (ONNX Runtime)** — fastest CPU throughput, BUT `ort` links the ONNX Runtime C++ lib; **no official static lib for all targets, effectively no musl prebuilt**, and on Windows it can pick up a stray `System32\onnxruntime.dll` and crash on version mismatch. The default "download binaries at build time" breaks in sandboxed/locked-down environments. This directly violates constraint #1.
- ✅ **Candle (HuggingFace, pure-Rust ML)** — runs BERT-family embedding models on CPU with **zero C/Python/system deps**. One `cargo build --target …` yields a truly portable binary across macOS/glibc/**musl**/Windows. Slower per-doc than ONNX, but it's a one-time, incremental, opt-in cost.
- ❌ **rust-bert (libtorch)** — needs multi-GB libtorch. Disqualified.

**Model:** `bge-small-en-v1.5` (384-dim, **~130 MB f32 weights**, 33 M params, MTEB ~62) as the default; `all-MiniLM-L6-v2` (384-dim, faster, MTEB ~56) as a lighter option — **same dimension, drop-in swap**. ⚠ **f32 is the plan of record** (quantized BERT in Candle is immature/often slower on CPU; int8 is a possible later optimization, not assumed). Use **CLS pooling** (token 0) for bge — NOT mean pooling. Downloaded from Hugging Face on first `--embed` run via the `hf-hub` crate (pin 0.5.0 or `ApiBuilder::with_cache_dir`; `rustls-tls`), cached to an XDG dir (`etcetera`/`cache_dir`); fully offline thereafter. ⚠ The `semantic` feature's **musl** build needs a musl C toolchain (candle-core force-enables tokenizers' `onig` C dep); the default keyword-only binary needs no *system* libs at runtime on any target (it does bundle SQLite + sqlite-vec C, compiled statically via `cc` — so a C cross-compiler is needed at build time, but plain C that links cleanly).

- **Honest cost:** embedding ~300k chunks with Candle on CPU is a **one-time job measured in minutes (tens of minutes worst case on older CPUs)** — hence opt-in + incremental + resumable, with an `indicatif` progress bar. Power users on supported platforms can opt into an `ort` **feature flag** for a fast path, but it is NEVER the default (it breaks portability).
- **Chunking:** ~500-token windows, ~15% overlap, aligned to turn boundaries.

Sources: ort linking docs / "bundling ONNX in Rust" (Nix/Docker pain) · fastembed-rs #6 · huggingface/candle · MTEB embedding benchmarks.

## 4. Vector storage + hybrid fusion — **sqlite-vec (static) + brute-force SIMD + RRF**

- **Storage:** `sqlite-vec` embeds its C source and **statically compiles into the binary via the `cc` crate** (registered via the 3-arg `ffi::sqlite3_auto_extension` / `register_auto_extension` — NOT the README's `fn()->i32` transmute) — no runtime `.so/.dylib/.dll`. Vectors live in the **same `.db` file** as FTS5 + metadata. Portability preserved.
- **Search at our scale:** ~300k × 384-d vectors (chunk-level; see IMPLEMENTATION §7 — vectors are keyed by chunk, not message). **ANN is unnecessary** — a brute-force cosine scan with `simsimd` (AVX2/AVX-512/NEON) is single-digit–to–tens of ms; int8 quantization (optional, later) can cut RAM ~4× / speed up scans. Exact recall, no index to build/maintain. (`hnsw_rs`, pure-Rust HNSW, is the documented fallback **only past ~1M vectors**.)
- **Hybrid fusion:** **Reciprocal Rank Fusion, k = 60** — run BM25 and vector scans independently, score each result `1/(k + rank)`, sum per document. No score normalization needed (ranks unify BM25's and cosine's incomparable scales). Optional lexical/semantic weights as multipliers (start 1.0 / 0.7).
- **Ranking polish:** composite = RRF × recency-decay (≈30-day half-life, cap +20%) × active-project boost; title/alias column weighted ≫ body.

Sources: alexgarcia.xyz/sqlite-vec/rust · SimSIMD · Azure/ParadeDB RRF.

## 5. Indexing performance

- **Parse:** `sonic-rs` (parse-into-struct, ~1.5–2× simd-json, 2–4 GB/s; lazy pointer-get to skip fields we don't need); `serde_json` fallback for non-SIMD targets. `memmap2` the file, split on `\n` to zero-copy `&[u8]` slices, `rayon` `par_iter` across lines and across files.
- **Bulk insert:** one prepared statement, batched in a single (or ~50–100k-row) transaction. PRAGMAs: `journal_mode=WAL`, `synchronous=NORMAL`, `cache_size=-262144` (256 MB), `temp_store=MEMORY`, `mmap_size=268435456`. This is the 10–20× win over naive autocommit; >100k inserts/s.
- **Incremental:** per-file `(path, mtime_ns, size, last_byte_offset)` meta row; skip unchanged by mtime+size; for append-only transcripts `seek()` to `last_byte_offset` and tail-read only new bytes → warm re-index near-instant.
- **Concurrency:** WAL allows concurrent readers + one writer; an app-level file lock guards two sessions racing to write. Reader tolerates a stale index ("indexing…").
- **Targets:** keyword cold index **2–4 s**; warm/incremental **near-instant**; search **< 10 ms** (keyword), **< 50 ms** (hybrid).

Sources: cloudwego/sonic-rs · memmap2 · rayon (#297 readahead caveat) · powersync/avi.im SQLite-insert benchmarks.

## 6. CLI / TUI UX (the "feel")

- **Interactive picker:** in-process with **`nucleo`** (the matcher behind Helix; ~6× faster than skim, fzf-style scoring, proper Unicode) + **`ratatui`** for the TUI. Type → instantly filtered → Enter to act, **< 10 ms/keystroke** over 300k items. No shelling out to `fzf` (process overhead, no shared state).
- **Non-interactive output:** `comfy-table` tables, colored snippets via `nu-ansi-term`, OSC-8 hyperlinks for clickable `file:line`, `--json` (serde) for the skill.
- **Onboarding:** `indicatif` progress during index/embed; `recall doctor` (DB integrity, SIMD support, transcript dir, model/network state — also the privacy-inspection command from `PLAN.md`); sensible zero-config defaults.
- **CLI:** `clap` (derive). Commands mirror `PLAN.md`: `index [--embed]`, `search [--semantic] [--json]`, `show <id> [--recovered] [--range/--head/--tail]`, `tree [project]`, `resume <id>`, `export <id> --format`, `name <id> "<alias>"`, `doctor`.

Sources: helix-editor/nucleo · ratatui.rs · comfy-table · indicatif.

## 7. Distribution — lowest-friction, every-platform

- **Build/release:** **`cargo-dist`** (use upstream **`axodotdev/cargo-dist`** — canonical, active through v0.32.0; Astral's temporary fork was re-merged upstream) generates the GitHub Actions release pipeline, per-target prebuilt binaries, checksums, and installers (shell, PowerShell, Homebrew, npm).
- **Cross-compile:** **`cargo-zigbuild`** (Zig linker) for static `*-unknown-linux-musl`, glibc-version-pinned gnu, and macOS `universal2`; `cross` (QEMU) only where Zig chokes on a bundled C dep — test both since we bundle SQLite + sqlite-vec C.
- **Targets:** macOS arm64+x64 (or universal2), `x86_64`/`aarch64-unknown-linux-musl` (static, distro-independent), `x86_64-pc-windows-msvc`.
- **Install channels (low→high friction):** Homebrew tap & `curl … | sh` (lowest, no prereqs) → `cargo-binstall` (prebuilt) / npx wrapper → GitHub Releases → `cargo install` (compiles, needs Rust). `cargo-dist` emits the first set automatically.
- **macOS notarization (the real blocker):** unsigned downloaded binaries are Gatekeeper-quarantined. Need Apple Developer ($99/yr) + Developer ID cert; `codesign --options runtime`, then `xcrun notarytool` — or sign+notarize **from Linux CI** via the `apple-codesign` (`rcodesign`) crate. `brew`/`curl|sh` installs sidestep the quarantine bit, reducing the pain.
- **Binary size:** `[profile.release] opt-level="z", lto="fat", codegen-units=1, panic="abort", strip=true` (min-sized-rust). Bundled SQLite ~1.5 MB; Candle adds model-runtime code but no external lib; expect a lean **~8–20 MB** binary (model cached separately on first `--embed`).

Sources: axodotdev/cargo-dist · cargo-zigbuild · cross · cargo-binstall · apple-codesign (rcodesign) · min-sized-rust · etcetera.

---

## 8. Decision summary

| Concern | Decision | Why (vs alternative) |
|---|---|---|
| Language | **Rust** | single static binary, speed, low memory; kills packaging risk |
| Storage + keyword | **rusqlite `bundled` + FTS5** | one portable `.db`, FTS5 compiled in, zero setup |
| Search engine | **FTS5 BM25** (Tantivy deferred) | fast enough at 300k, one file > two index formats |
| Embeddings | **Candle (pure-Rust) + bge-small-en-v1.5 f32, CLS pooling** | only path that runs on musl/all targets with no system lib (ort fails portability) |
| Embedding delivery | **download on first `--embed`**, `hf-hub`, XDG cache | small binary, offline after first run |
| Vector store | **sqlite-vec (static) in the same `.db`** | one file, no runtime extension, cross-platform |
| ANN | **brute-force SIMD (`simsimd`, f32)** | exact recall, <50 ms at 300k; HNSW only past ~1M |
| Hybrid | **RRF, k=60** | rank-based, no score normalization, robust |
| Parse | **sonic-rs + memmap2 + rayon** | 2–4 GB/s; cold index 2–4 s |
| Bulk insert | **batched txn + WAL/NORMAL PRAGMAs** | 10–20× over naive |
| Picker UX | **nucleo + ratatui** | <10 ms/keystroke, in-process, fzf-class |
| Output | comfy-table + nu-ansi-term + OSC-8 + `--json` | readable + skill-friendly |
| Distribution | **cargo-dist (axodotdev) + cargo-zigbuild/cross** | auto installers, musl + macOS universal2 + Windows |
| Install | brew / curl\|sh / cargo-binstall | near-zero friction |

## 9. Proposed `Cargo.toml` (core deps)
```toml
[dependencies]
rusqlite   = { version = "0.39", features = ["bundled"] }   # matched to r2d2_sqlite 0.34 (one libsqlite3-sys)
sqlite-vec = "*"          # static, registered via sqlite3_auto_extension
sonic-rs   = "*"          # serde_json fallback for non-SIMD targets
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
memmap2    = "*"
rayon      = "*"
clap       = { version = "4", features = ["derive"] }
simsimd    = "*"
indicatif  = "*"
comfy-table = "*"
nu-ansi-term = "*"
etcetera    = "*"   # XDG on Linux/macOS + Known Folders on Windows (not `directories`)

# Tier-2 semantic (feature = "semantic", opt-in):
candle-core = "*"
candle-transformers = "*"
hf-hub = "*"
tokenizers = "*"

# Interactive picker (feature = "tui"):
nucleo  = "*"
ratatui = "*"

[features]
default  = []     # lean/headless/plugin-friendly; the plugin+hook path never needs the TUI
tui      = ["nucleo", "ratatui"]   # interactive picker, opt-in
semantic = ["candle-core", "candle-transformers", "hf-hub", "tokenizers"]
fast-embed = []   # opt-in ort/ONNX path for power users; NOT portable, never default

[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```
(Versions pinned during Phase 1; `"*"` here is a placeholder for the latest at build time.)

---

## 10. Open technical risks (carry into build)
1. **musl + bundled C deps** (SQLite + sqlite-vec) — verify static musl build in CI early; cargo-zigbuild vs cross.
2. **Candle embedding throughput** — benchmark on a mid CPU; if "tens of minutes" for 300k is too slow, ship int8 + smaller model (MiniLM) as default and document the `fast-embed` opt-in.
3. **macOS notarization from CI** — prove `rcodesign` flow before first public release.
4. **sqlite-vec static link on Windows MSVC** — confirm the `cc` build works on the MSVC target.
5. **FTS5 external-content consistency** — covered by Phase 0 delete/modify/truncate tests.
