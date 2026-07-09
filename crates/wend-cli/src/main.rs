//! wend CLI entry point.

mod cli;
mod render;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use wend_core::model::Block;
use wend_core::store::{SearchHit, SessionRef, Store};
use wend_core::{config, index, search};

fn main() -> std::process::ExitCode {
    let args = Cli::parse();
    init_logging(args.verbose);

    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Cli) -> Result<()> {
    match args.command {
        Command::Index {
            incremental,
            include_subagents,
            embed,
        } => {
            if include_subagents {
                tracing::warn!("--include-subagents not yet implemented; indexing top-level only");
            }
            let db = config::index_db_path()?;
            let projects = config::projects_dir()?;
            let mut store =
                Store::open(&db).with_context(|| format!("opening index at {}", db.display()))?;
            let stats = index::index_all(&mut store, &projects, incremental)
                .with_context(|| format!("indexing {}", projects.display()))?;
            println!(
                "indexed {} session(s) — {} unchanged, {} files seen, {} bad lines skipped",
                stats.indexed, stats.skipped_unchanged, stats.files_seen, stats.parse_skipped_lines
            );
            if embed {
                run_embed(&mut store)?;
            }
            println!("index: {}", db.display());
            Ok(())
        }

        Command::Search {
            query,
            semantic,
            json,
            role,
            limit,
        } => {
            let db = config::index_db_path()?;
            let store =
                Store::open(&db).with_context(|| format!("opening index at {}", db.display()))?;
            let role = role.map(|r| r.as_db_str());
            let hits = if semantic {
                if role.is_some() {
                    tracing::warn!(
                        "--role only applies to keyword search; ignored with --semantic"
                    );
                }
                run_semantic(&store, &query, limit)?
            } else {
                search::search(&store, &query, limit, role)?
            };
            if json {
                println!("{}", serde_json::to_string(&hits)?);
            } else if hits.is_empty() {
                if store.session_count().unwrap_or(0) == 0 {
                    println!("index is empty — run `wend index` first");
                } else {
                    println!("no matches for {query:?}");
                }
            } else {
                for (i, h) in hits.iter().enumerate() {
                    let title = if h.title.is_empty() {
                        "(untitled)"
                    } else {
                        &h.title
                    };
                    println!("{}. [{}] {} · {}", i + 1, h.project, title, h.session_id);
                    println!("    {}", h.snippet);
                }
            }
            Ok(())
        }

        Command::Messages { role, json, limit } => {
            let store = open_store()?;
            let msgs = store.list_prose_messages(role.as_db_str(), limit)?;
            if json {
                println!("{}", serde_json::to_string(&msgs)?);
            } else {
                // Group by session so the conversation flow stays legible.
                let mut cur = String::new();
                for m in &msgs {
                    if m.session_id != cur {
                        let title = if m.title.is_empty() {
                            "(untitled)"
                        } else {
                            &m.title
                        };
                        println!("\n── {} · {} · {} ──", title, m.project, m.session_id);
                        cur = m.session_id.clone();
                    }
                    println!("{}", m.text);
                }
                if msgs.is_empty() {
                    println!("no messages — run `wend index` first");
                }
            }
            Ok(())
        }

        Command::Doctor => {
            let db = config::index_db_path()?;
            let projects = config::projects_dir()?;
            println!("wend {}", wend_core::VERSION);
            println!("projects dir: {}", projects.display());
            println!("index db:     {}", db.display());
            if db.exists() {
                let store = Store::open(&db)?;
                println!("sessions:     {}", store.session_count()?);
                println!("messages:     {}", store.message_count()?);
                let embedded = store.chunk_vector_count().unwrap_or(0);
                let semantic_build = cfg!(feature = "semantic");
                println!(
                    "semantic:     {} ({} chunk(s) embedded)",
                    if semantic_build {
                        "built-in"
                    } else {
                        "not in this build (rebuild with --features semantic)"
                    },
                    embedded
                );
            } else {
                println!("index:        not built — run `wend index`");
            }
            Ok(())
        }

        Command::Show {
            id,
            recovered,
            head,
            tail,
            range,
            count,
        } => {
            let store = open_store()?;
            let sess = resolve_or_report(&store, &id)?;

            // Build printable chunks. With --recovered we interleave boundary
            // markers and flag the pre-compaction history the live UI hides.
            let mut chunks: Vec<String> = Vec::new();
            let mut header_extra: Option<String> = None;
            if recovered {
                let rec = wend_core::recover::recover_session(&store, sess.pk)?;
                let mut h = format!(
                    "  recovered: {} pre-compaction message(s) across {} boundary(ies) hidden by the live UI",
                    rec.recovered_count, rec.boundary_count
                );
                if rec.cross_file_boundaries > 0 {
                    h.push_str(&format!(
                        "\n  note: {} boundary(ies) continue from another session file — that earliest history isn't included here",
                        rec.cross_file_boundaries
                    ));
                }
                header_extra = Some(h);
                for item in &rec.items {
                    match item {
                        wend_core::recover::Item::Boundary(b) => {
                            chunks.push(format!(
                                "─── ⟪ compaction boundary · {} · {}→{} tokens ⟫ ───",
                                b.trigger.as_deref().unwrap_or("?"),
                                opt_num(b.pre_tokens),
                                opt_num(b.post_tokens),
                            ));
                        }
                        wend_core::recover::Item::Message(rm) => {
                            if let Some(chunk) = render_message_chunk(
                                &rm.row.content_json,
                                &rm.row.role,
                                &rm.row.rec_type,
                                rm.pre_compaction,
                            ) {
                                chunks.push(chunk);
                            }
                        }
                    }
                }
            } else {
                for m in &store.session_messages(sess.pk)? {
                    if let Some(chunk) =
                        render_message_chunk(&m.content_json, &m.role, &m.rec_type, false)
                    {
                        chunks.push(chunk);
                    }
                }
            }

            let total = chunks.len();

            // --count: just the number.
            if count {
                println!("{total}");
                return Ok(());
            }

            // Pick the window [start..end) (0-based). Priority: range > head >
            // tail > a soft default cap so a huge session doesn't flood the term.
            const DEFAULT_CAP: usize = 200;
            let (start, end, note): (usize, usize, Option<String>) = if let Some(r) = &range {
                let (a, b) = parse_range(r)?; // 1-based inclusive
                let s = a.saturating_sub(1).min(total);
                let e = b.min(total).max(s);
                (s, e, Some(format!("messages {}–{} of {total}", s + 1, e)))
            } else if let Some(n) = head {
                let e = n.min(total);
                (0, e, Some(format!("first {e} of {total}")))
            } else if let Some(n) = tail {
                let s = total.saturating_sub(n);
                (s, total, Some(format!("last {} of {total}", total - s)))
            } else if total > DEFAULT_CAP {
                // --recovered: the hidden early history is what you want → from start.
                if recovered {
                    (
                        0,
                        DEFAULT_CAP,
                        Some(format!("first {DEFAULT_CAP} of {total}")),
                    )
                } else {
                    (
                        total - DEFAULT_CAP,
                        total,
                        Some(format!("last {DEFAULT_CAP} of {total}")),
                    )
                }
            } else {
                (0, total, None)
            };

            let title = if sess.title.is_empty() {
                "(untitled)"
            } else {
                &sess.title
            };
            println!("# {title}  [{}]", sess.session_id);
            if let Some(p) = &sess.project_path {
                println!("  project: {p}");
            }
            println!("  {total} messages");
            if let Some(h) = header_extra {
                println!("{h}");
            }
            if let Some(note) = note {
                println!("  showing {note}  (--head N / --tail N / --range A:B / --count, or `export` for all)");
            }
            for (i, c) in chunks[start..end].iter().enumerate() {
                println!("\n[{}] {c}", start + i + 1);
            }
            Ok(())
        }

        Command::Resume { id } => {
            let store = open_store()?;
            let sess = resolve_or_report(&store, &id)?;
            match &sess.project_path {
                Some(cwd) => {
                    if !std::path::Path::new(cwd).is_dir() {
                        eprintln!(
                            "warning: project dir no longer exists: {cwd} (the cd will fail — resume manually from another dir if needed)"
                        );
                    }
                    println!(
                        "cd {} && claude --resume {}",
                        shell_quote(cwd),
                        sess.session_id
                    );
                }
                None => println!("claude --resume {}", sess.session_id),
            }
            Ok(())
        }

        Command::Name { id, alias } => {
            if alias.trim().is_empty() {
                anyhow::bail!("alias cannot be empty");
            }
            let mut store = open_store()?;
            let sess = resolve_or_report(&store, &id)?;
            store.set_custom_title(sess.pk, &alias)?;
            println!("named {} → {alias:?}", short(&sess.session_id));
            Ok(())
        }

        Command::Tree { project } => {
            use wend_core::topology::{self, Confidence};
            let store = open_store()?;
            let topo = topology::build(&store, project.as_deref())?;
            if topo.projects.is_empty() {
                match &project {
                    Some(p) => println!("no sessions match {p:?}"),
                    None => println!("no sessions indexed — run `wend index`"),
                }
                return Ok(());
            }
            for proj in &topo.projects {
                let n: usize = proj.main_sessions.len()
                    + proj
                        .worktrees
                        .iter()
                        .map(|w| w.sessions.len())
                        .sum::<usize>();
                println!("\n{}  ({n} session(s))", proj.repo);
                for s in &proj.main_sessions {
                    println!(
                        "  ├─ {}  {}  · {} msgs",
                        short(&s.session_id),
                        title_or(&s.title),
                        s.message_count
                    );
                }
                for w in &proj.worktrees {
                    let conf = match w.confidence {
                        Confidence::Explicit => "explicit",
                        Confidence::Inferred => "inferred",
                    };
                    let branch = w.branch.as_deref().unwrap_or("?");
                    println!("  ├─ ⌥ worktree: {} (branch {branch}) [{conf}]", w.name);
                    for s in &w.sessions {
                        println!(
                            "  │    └─ {}  {}  · {} msgs",
                            short(&s.session_id),
                            title_or(&s.title),
                            s.message_count
                        );
                    }
                }
            }
            Ok(())
        }
        Command::Export { .. } => {
            anyhow::bail!(
                "`export` is not implemented yet — use `wend show <id>` to read a transcript for now"
            )
        }
    }
}

fn open_store() -> Result<Store> {
    let db = config::index_db_path()?;
    Store::open(&db).with_context(|| format!("opening index at {}", db.display()))
}

#[cfg(feature = "semantic")]
fn run_embed(store: &mut Store) -> Result<()> {
    let threads = wend_core::embed::embed_threads();
    eprintln!(
        "building semantic index — CPU-bound, using {threads} thread(s) \
         (set WEND_EMBED_THREADS to change; first run also downloads the model)…"
    );
    let (created, embedded) = wend_core::embed::build_index(store)?;
    println!("semantic: {created} new chunk(s) created, {embedded} embedded");
    Ok(())
}

#[cfg(not(feature = "semantic"))]
fn run_embed(_store: &mut Store) -> Result<()> {
    tracing::warn!("--embed needs a build with --features semantic; skipping");
    Ok(())
}

#[cfg(feature = "semantic")]
fn run_semantic(store: &Store, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    Ok(wend_core::embed::hybrid_search(store, query, limit)?)
}

#[cfg(not(feature = "semantic"))]
fn run_semantic(store: &Store, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    tracing::warn!("--semantic needs a build with --features semantic; keyword only");
    Ok(search::search(store, query, limit, None)?)
}

/// Resolve a short session-id prefix to exactly one session, or report candidates.
fn resolve_or_report(store: &Store, id: &str) -> Result<SessionRef> {
    let candidates = store.find_sessions(id, 25)?;
    match candidates.len() {
        0 => anyhow::bail!("no session matching '{id}' — run `wend search` to find one"),
        1 => Ok(candidates.into_iter().next().unwrap()),
        n => {
            // an exact full-id match disambiguates
            if let Some(exact) = candidates.iter().find(|s| s.session_id == id) {
                return Ok(exact.clone());
            }
            eprintln!("ambiguous id '{id}' — {n} sessions match:");
            for c in &candidates {
                let title = if c.title.is_empty() {
                    "(untitled)"
                } else {
                    &c.title
                };
                eprintln!(
                    "  {}  [{}]  {title}",
                    short(&c.session_id),
                    c.project_path.as_deref().unwrap_or("?")
                );
            }
            anyhow::bail!("disambiguate with a longer id prefix");
        }
    }
}

fn short(session_id: &str) -> &str {
    &session_id[..session_id.len().min(8)]
}

fn title_or(t: &str) -> &str {
    if t.is_empty() {
        "(untitled)"
    } else {
        t
    }
}

/// Parse a 1-based inclusive range like `10:20` or `10-20` into `(start, end)`.
fn parse_range(s: &str) -> Result<(usize, usize)> {
    let parts: Vec<&str> = s.splitn(2, [':', '-']).collect();
    if parts.len() != 2 {
        anyhow::bail!("range must be A:B, e.g. 10:20");
    }
    let a: usize = parts[0]
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("bad range start: {:?}", parts[0]))?;
    let b: usize = parts[1]
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("bad range end: {:?}", parts[1]))?;
    if a < 1 {
        anyhow::bail!("range is 1-based; start must be >= 1");
    }
    if b < a {
        anyhow::bail!("range end ({b}) is before start ({a})");
    }
    Ok((a, b))
}

/// Render one stored message into a printable chunk, or `None` if it has no
/// visible body (empty graph nodes like system/progress).
fn render_message_chunk(
    content_json: &str,
    role: &Option<String>,
    rec_type: &str,
    pre_compaction: bool,
) -> Option<String> {
    let blocks: Vec<Block> = serde_json::from_str(content_json).unwrap_or_default();
    let body = render::render_blocks(&blocks);
    if body.trim().is_empty() {
        return None;
    }
    let who = role.clone().unwrap_or_else(|| rec_type.to_string());
    let tag = if pre_compaction {
        "  ⟨pre-compaction · hidden by UI⟩"
    } else {
        ""
    };
    Some(format!("## {who}{tag}\n{}", body.trim_end()))
}

fn opt_num(n: Option<i64>) -> String {
    n.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string())
}

/// Minimal shell quoting for the emitted `cd` command.
fn shell_quote(s: &str) -> String {
    if s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || "'\"\\$`".contains(c))
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

/// Initialize tracing to **stderr** (stdout is reserved for command output/JSON,
/// so the skill and shell pipes stay clean).
fn init_logging(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};

    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_env("WEND_LOG").unwrap_or_else(|_| EnvFilter::new(default_level));

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn parse_range_accepts_colon_and_dash() {
        assert_eq!(parse_range("3:5").unwrap(), (3, 5));
        assert_eq!(parse_range("10-20").unwrap(), (10, 20));
        assert_eq!(parse_range(" 1 : 9 ").unwrap(), (1, 9));
    }

    #[test]
    fn parse_range_rejects_bad_input() {
        assert!(parse_range("0:5").is_err()); // 1-based
        assert!(parse_range("20:5").is_err()); // end < start
        assert!(parse_range("abc").is_err());
        assert!(parse_range("5").is_err());
    }
}
