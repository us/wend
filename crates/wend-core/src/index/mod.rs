//! Indexing: discover top-level session files, assemble them, and write them
//! into the [`Store`] idempotently.

use crate::error::Result;
use crate::model::{
    BoundaryRecord, BridgeRecord, MessageRecord, Routed, TitleUpdate, WorktreeRecord,
};
use crate::parser::parse_file;
use crate::store::{FileStat, Store};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// A fully parsed session ready to persist.
#[derive(Debug)]
pub struct AssembledSession {
    pub session_id: String,
    pub source_kind: String,
    pub file_path: String,
    pub project_path: Option<String>,
    pub project_name: Option<String>,
    pub git_branch: Option<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub ai_title: Option<String>,
    pub custom_title: Option<String>,
    pub title: String,
    pub has_compaction: bool,
    pub messages: Vec<MessageRecord>,
    pub boundaries: Vec<BoundaryRecord>,
    pub worktrees: Vec<WorktreeRecord>,
    pub bridges: Vec<BridgeRecord>,
    pub file_mtime_ns: i64,
    pub file_size: i64,
}

/// Outcome of an indexing run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    pub files_seen: usize,
    pub indexed: usize,
    pub skipped_unchanged: usize,
    pub parse_skipped_lines: usize,
}

/// Assemble a parsed file into a persistable session.
pub fn assemble(
    parsed: crate::parser::ParsedFile,
    file_path: String,
    session_id: String,
    stat: FileStat,
    fallback_name: Option<String>,
) -> AssembledSession {
    let mut messages = Vec::new();
    let mut boundaries = Vec::new();
    let mut worktrees = Vec::new();
    let mut bridges = Vec::new();
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut cwd: Option<String> = None;

    let mut track = |ts: Option<i64>| {
        if let Some(ts) = ts {
            first_ts = Some(first_ts.map_or(ts, |f: i64| f.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |l: i64| l.max(ts)));
        }
    };

    for (_line, rec) in parsed.records {
        match rec {
            Routed::Message(m) => {
                track(m.ts);
                if cwd.is_none() {
                    if let Some(c) = &m.cwd {
                        cwd = Some(c.clone());
                    }
                }
                messages.push(m);
            }
            Routed::Boundary(b) => {
                track(b.ts);
                boundaries.push(b);
            }
            Routed::Worktree(w) => worktrees.push(w),
            Routed::Bridge(b) => bridges.push(b),
            Routed::Title(TitleUpdate::Ai(t)) => ai_title = Some(t),
            Routed::Title(TitleUpdate::Custom(t)) => custom_title = Some(t),
            Routed::Skip => {}
        }
    }

    let has_compaction = !boundaries.is_empty();
    let project_path = cwd;
    // Prefer the cwd basename; fall back to the (encoded) project dir name so a
    // session with no cwd line still has a non-empty project label (spec §0).
    let project_name = project_path
        .as_ref()
        .and_then(|p| {
            Path::new(p)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .or(fallback_name);
    let title = custom_title
        .clone()
        .or_else(|| ai_title.clone())
        .unwrap_or_default();

    AssembledSession {
        session_id,
        source_kind: "top_level".to_string(),
        file_path,
        project_path,
        project_name,
        git_branch: None,
        first_ts,
        last_ts,
        ai_title,
        custom_title,
        title,
        has_compaction,
        messages,
        boundaries,
        worktrees,
        bridges,
        file_mtime_ns: stat.mtime_ns,
        file_size: stat.size,
    }
}

/// Discover top-level session files: `projects_dir/<encoded>/<session>.jsonl`.
/// Single-level only — files under `<session>/subagents` etc. are NOT included.
pub fn discover_top_level(projects_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(_) => return Ok(out), // missing dir → nothing to index
    };
    for project in entries.flatten() {
        let pdir = project.path();
        if !pdir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&pdir) else {
            continue;
        };
        for f in files.flatten() {
            let fp = f.path();
            if fp.is_file() && fp.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(fp);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Index all top-level sessions under `projects_dir` into `store`.
///
/// Parsing + assembly run in parallel (`rayon`); the SQLite writes are serial
/// through the single connection.
pub fn index_all(store: &mut Store, projects_dir: &Path, incremental: bool) -> Result<IndexStats> {
    use rayon::prelude::*;

    let paths = discover_top_level(projects_dir)?;
    let mut stats = IndexStats {
        files_seen: paths.len(),
        ..IndexStats::default()
    };

    // Serial pass: stat each file and drop unchanged ones (incremental).
    let mut worklist: Vec<(PathBuf, String, FileStat)> = Vec::new();
    for path in paths {
        let meta = std::fs::metadata(&path)?;
        let stat = FileStat {
            mtime_ns: mtime_ns(&meta),
            size: meta.len() as i64,
        };
        let file_path = path.to_string_lossy().into_owned();
        if incremental {
            if let Some(prev) = store.file_stat(&file_path)? {
                if prev == stat {
                    stats.skipped_unchanged += 1;
                    continue;
                }
            }
        }
        worklist.push((path, file_path, stat));
    }

    // Parallel pass: parse + assemble (no DB access here).
    let assembled: Vec<Result<(AssembledSession, usize)>> = worklist
        .into_par_iter()
        .map(|(path, file_path, stat)| {
            let parsed = parse_file(&path)?;
            let skipped = parsed.skipped_count();
            let session_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let fallback_name = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned());
            Ok((
                assemble(parsed, file_path, session_id, stat, fallback_name),
                skipped,
            ))
        })
        .collect();

    // Serial pass: write into the store.
    for result in assembled {
        let (session, skipped) = result?;
        stats.parse_skipped_lines += skipped;
        store.replace_session(&session)?;
        stats.indexed += 1;
    }
    Ok(stats)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
