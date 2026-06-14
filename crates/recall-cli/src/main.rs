//! session-recall CLI entry point.

mod cli;
mod render;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use recall_core::model::Block;
use recall_core::store::{SessionRef, Store};
use recall_core::{config, index, search};

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
            if embed {
                tracing::warn!("--embed not yet implemented");
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
            println!("index: {}", db.display());
            Ok(())
        }

        Command::Search {
            query,
            semantic,
            json,
            limit,
        } => {
            if semantic {
                tracing::warn!("--semantic not yet implemented; keyword only");
            }
            let db = config::index_db_path()?;
            let store =
                Store::open(&db).with_context(|| format!("opening index at {}", db.display()))?;
            let hits = search::search(&store, &query, limit)?;
            if json {
                println!("{}", serde_json::to_string(&hits)?);
            } else if hits.is_empty() {
                println!("no matches for {query:?}");
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

        Command::Doctor => {
            let db = config::index_db_path()?;
            let projects = config::projects_dir()?;
            println!("recall {}", recall_core::VERSION);
            println!("projects dir: {}", projects.display());
            println!("index db:     {}", db.display());
            if db.exists() {
                let store = Store::open(&db)?;
                println!("sessions:     {}", store.session_count()?);
                println!("messages:     {}", store.message_count()?);
            } else {
                println!("index:        not built — run `recall index`");
            }
            Ok(())
        }

        Command::Show {
            id,
            recovered,
            head,
            tail,
        } => {
            let store = open_store()?;
            let sess = resolve_or_report(&store, &id)?;

            // Build printable chunks. With --recovered we interleave boundary
            // markers and flag the pre-compaction history the live UI hides.
            let mut chunks: Vec<String> = Vec::new();
            let mut header_extra: Option<String> = None;
            if recovered {
                let rec = recall_core::recover::recover_session(&store, sess.pk)?;
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
                        recall_core::recover::Item::Boundary(b) => {
                            chunks.push(format!(
                                "─── ⟪ compaction boundary · {} · {}→{} tokens ⟫ ───",
                                b.trigger.as_deref().unwrap_or("?"),
                                opt_num(b.pre_tokens),
                                opt_num(b.post_tokens),
                            ));
                        }
                        recall_core::recover::Item::Message(rm) => {
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

            // Window: explicit --head/--tail, else a soft default cap so a huge
            // session doesn't dump tens of thousands of lines.
            const DEFAULT_CAP: usize = 200;
            let total = chunks.len();
            let mut note: Option<(&str, usize)> = None;
            let slice: &[String] = match (head, tail) {
                (Some(n), _) => {
                    let n = n.min(total);
                    note = Some(("first", n));
                    &chunks[..n]
                }
                (None, Some(n)) => {
                    let n = n.min(total);
                    note = Some(("last", n));
                    &chunks[total - n..]
                }
                (None, None) if total > DEFAULT_CAP => {
                    // With --recovered the user wants the hidden *early* history,
                    // so default to the start; otherwise show the most recent.
                    if recovered {
                        note = Some(("first", DEFAULT_CAP));
                        &chunks[..DEFAULT_CAP]
                    } else {
                        note = Some(("last", DEFAULT_CAP));
                        &chunks[total - DEFAULT_CAP..]
                    }
                }
                (None, None) => &chunks[..],
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
            if let Some(h) = header_extra {
                println!("{h}");
            }
            if let Some((which, n)) = note {
                println!(
                    "  showing {which} {n} of {total} blocks (use --head/--tail, or `export` for the full transcript)"
                );
            }
            for c in slice {
                println!("\n{c}");
            }
            Ok(())
        }

        Command::Resume { id } => {
            let store = open_store()?;
            let sess = resolve_or_report(&store, &id)?;
            match &sess.project_path {
                Some(cwd) => println!(
                    "cd {} && claude --resume {}",
                    shell_quote(cwd),
                    sess.session_id
                ),
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

        other => anyhow::bail!("command not yet implemented: {other:?}"),
    }
}

fn open_store() -> Result<Store> {
    let db = config::index_db_path()?;
    Store::open(&db).with_context(|| format!("opening index at {}", db.display()))
}

/// Resolve a short session-id prefix to exactly one session, or report candidates.
fn resolve_or_report(store: &Store, id: &str) -> Result<SessionRef> {
    let candidates = store.find_sessions(id, 25)?;
    match candidates.len() {
        0 => anyhow::bail!("no session matching '{id}' — run `recall search` to find one"),
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
        EnvFilter::try_from_env("RECALL_LOG").unwrap_or_else(|_| EnvFilter::new(default_level));

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
