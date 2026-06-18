//! Compaction recovery — the ⭐ feature.
//!
//! After Claude compacts a session, the live UI shows only the post-compaction
//! summary and everything after it; the pre-compaction turns are hidden. We index
//! the entire `.jsonl` (including those hidden turns), so recovery reconstructs
//! the full ordered transcript and marks which messages the live UI no longer
//! shows. File order is chronological (verified), so a line-ordered merge of
//! messages + boundary markers reproduces the true history for an in-file session.
//!
//! Limitation: when a session was resumed from another file (a `--resume`/`bg`
//! continuation, ~0.3% of sessions), a boundary's `logical_parent_uuid` points at
//! a message in that other file. We can't reconstruct that earliest cross-file
//! history from this session alone, so we detect and report it
//! ([`Recovered::cross_file_boundaries`]) instead of silently truncating.

use crate::error::Result;
use crate::store::{MessageRow, Store};

/// A compaction boundary, rendered inline between messages.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryMarker {
    pub trigger: Option<String>,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
}

/// A message in the reconstructed transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredMessage {
    pub row: MessageRow,
    /// True if this message precedes the last compaction boundary — i.e. the live
    /// UI no longer shows it.
    pub pre_compaction: bool,
}

/// One item in the ordered, reconstructed transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Message(RecoveredMessage),
    Boundary(BoundaryMarker),
}

/// The result of recovering a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovered {
    pub items: Vec<Item>,
    pub boundary_count: usize,
    /// Number of pre-(last-)compaction messages — the history the live UI hides.
    pub recovered_count: usize,
    /// Boundaries whose `logical_parent_uuid` lives in another file (cross-session
    /// resume) — their earliest history can't be reconstructed from this session.
    pub cross_file_boundaries: usize,
}

/// Reconstruct the full transcript for a session, including hidden pre-compaction
/// history, with boundary markers in place.
pub fn recover_session(store: &Store, session_pk: i64) -> Result<Recovered> {
    let messages = store.session_messages(session_pk)?;
    let boundaries = store.session_boundaries(session_pk)?;
    let last_boundary_line = boundaries.iter().map(|b| b.line_no).max();

    // A boundary whose logical parent isn't in this file is a cross-file resume
    // (its earliest history lives in another session file).
    let uuids = store.session_message_uuids(session_pk)?;
    let cross_file_boundaries = boundaries
        .iter()
        .filter(|b| {
            b.logical_parent_uuid
                .as_ref()
                .is_some_and(|lp| !uuids.contains(lp))
        })
        .count();

    let mut items = Vec::with_capacity(messages.len() + boundaries.len());
    let mut recovered_count = 0usize;
    let mut bi = 0usize;

    for m in messages {
        // Emit any boundaries that occur at or before this message's line.
        while bi < boundaries.len() && boundaries[bi].line_no <= m.line_no {
            let b = &boundaries[bi];
            items.push(Item::Boundary(BoundaryMarker {
                trigger: b.trigger.clone(),
                pre_tokens: b.pre_tokens,
                post_tokens: b.post_tokens,
            }));
            bi += 1;
        }
        let pre = last_boundary_line.is_some_and(|lb| m.line_no < lb);
        if pre {
            recovered_count += 1;
        }
        items.push(Item::Message(RecoveredMessage {
            row: m,
            pre_compaction: pre,
        }));
    }
    // Any boundaries after the last message (unusual, but keep them).
    while bi < boundaries.len() {
        let b = &boundaries[bi];
        items.push(Item::Boundary(BoundaryMarker {
            trigger: b.trigger.clone(),
            pre_tokens: b.pre_tokens,
            post_tokens: b.post_tokens,
        }));
        bi += 1;
    }

    Ok(Recovered {
        items,
        boundary_count: boundaries.len(),
        recovered_count,
        cross_file_boundaries,
    })
}
