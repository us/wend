//! Worktree/session topology — group sessions by repo and surface git-worktree
//! relationships, the one view no competing tool offers.
//!
//! A worktree is detected two ways: explicitly from a `worktree-state` record
//! (high confidence, carries branch + original repo), or inferred from a cwd that
//! contains `/.claude/worktrees/<name>` (lower confidence). Sessions with neither
//! are listed directly under their repo.
//!
//! Subagent topology is deferred: subagents aren't indexed by default
//! (`index --include-subagents` is not implemented yet).

use crate::error::Result;
use crate::store::{SessionBrief, Store};
use std::collections::BTreeMap;

/// Link confidence for a worktree grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// From a `worktree-state` record.
    Explicit,
    /// Inferred from the cwd path shape.
    Inferred,
}

/// A worktree under a repo, with its sessions.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeNode {
    pub name: String,
    pub branch: Option<String>,
    pub confidence: Confidence,
    pub sessions: Vec<SessionBrief>,
}

/// A repo root with its direct sessions and worktrees.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectNode {
    pub repo: String,
    pub main_sessions: Vec<SessionBrief>,
    pub worktrees: Vec<WorktreeNode>,
}

/// The full topology.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Topology {
    pub projects: Vec<ProjectNode>,
}

/// Classify one session's location: `(repo_root, Option<(worktree_name, branch, confidence)>)`.
fn classify(
    s: &SessionBrief,
    wt: Option<&crate::store::WorktreeInfo>,
) -> (String, Option<(String, Option<String>, Confidence)>) {
    let cwd = s
        .project_path
        .clone()
        .unwrap_or_else(|| "(unknown)".to_string());

    // Explicit worktree-state record wins.
    if let Some(info) = wt {
        if let Some(repo) = info.original_cwd.clone() {
            let name = info
                .worktree_name
                .clone()
                .unwrap_or_else(|| basename(&cwd).to_string());
            return (
                repo,
                Some((name, info.branch.clone(), Confidence::Explicit)),
            );
        }
    }

    // Inferred: .../.claude/worktrees/<name>[/...]
    if let Some(idx) = cwd.find("/.claude/worktrees/") {
        let repo = cwd[..idx].to_string();
        let rest = &cwd[idx + "/.claude/worktrees/".len()..];
        let name = rest.split('/').next().unwrap_or(rest).to_string();
        return (repo, Some((name, None, Confidence::Inferred)));
    }

    (cwd, None)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build the topology, optionally filtered to repos/sessions matching `filter`
/// (substring match on repo path, project name, or title).
pub fn build(store: &Store, filter: Option<&str>) -> Result<Topology> {
    let sessions = store.all_sessions()?;
    let mut wt_by_pk = std::collections::HashMap::new();
    for w in store.all_worktrees()? {
        wt_by_pk.insert(w.session_pk, w); // last record per session wins
    }

    // repo -> (main sessions, worktree_name -> node)
    struct Builder {
        main: Vec<SessionBrief>,
        worktrees: BTreeMap<String, WorktreeNode>,
    }
    let mut repos: BTreeMap<String, Builder> = BTreeMap::new();

    for s in sessions {
        let (repo, wt) = classify(&s, wt_by_pk.get(&s.pk));

        // Apply filter (against repo, project name, or title).
        if let Some(f) = filter {
            let fl = f.to_lowercase();
            let hay = format!(
                "{} {} {}",
                repo.to_lowercase(),
                s.project_name.clone().unwrap_or_default().to_lowercase(),
                s.title.to_lowercase()
            );
            if !hay.contains(&fl) {
                continue;
            }
        }

        let builder = repos.entry(repo).or_insert_with(|| Builder {
            main: Vec::new(),
            worktrees: BTreeMap::new(),
        });
        match wt {
            None => builder.main.push(s),
            Some((name, branch, confidence)) => {
                let node = builder
                    .worktrees
                    .entry(name.clone())
                    .or_insert_with(|| WorktreeNode {
                        name,
                        branch: branch.clone(),
                        confidence,
                        sessions: Vec::new(),
                    });
                // Prefer an explicit branch/confidence if a later record has it.
                if node.branch.is_none() {
                    node.branch = branch;
                }
                if confidence == Confidence::Explicit {
                    node.confidence = Confidence::Explicit;
                }
                node.sessions.push(s);
            }
        }
    }

    let projects = repos
        .into_iter()
        .map(|(repo, b)| ProjectNode {
            repo,
            main_sessions: b.main,
            worktrees: b.worktrees.into_values().collect(),
        })
        .collect();

    Ok(Topology { projects })
}
