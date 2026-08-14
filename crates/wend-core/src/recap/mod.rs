//! Reading the compaction summaries Claude Code already wrote.
//!
//! When a session runs out of context, Claude Code compacts it and injects a
//! structured summary as the next message. `wend` has always stored those rows
//! (`messages.is_compact_summary = 1`) and never looked inside them.
//!
//! They answer the question a transcript search cannot: *what did I ask for at
//! the start, and what is still open?* On one real corpus, 419 of 420 summaries
//! carry the same nine headings, and `Primary Request and Intent` even tracks how
//! the goal evolved mid-session, quoting the user's own words.
//!
//! So this module parses rather than generates. The expensive summarization pass
//! is already paid for; re-deriving it with another model would cost tokens to
//! produce something worse.
//!
//! Coverage is uneven and that shapes the feature: measured over 759 sessions,
//! only 1% of sessions under 10 spoken turns have a summary, against 80% of
//! those with 50+. Short sessions have no arc to recover and need nothing; long
//! ones are where this pays, and that is where the coverage is.

use crate::error::Result;
use crate::store::Store;

/// The headings Claude Code's compaction template emits, in template order.
pub const HEADINGS: [&str; 9] = [
    "Primary Request and Intent",
    "Key Technical Concepts",
    "Files and Code Sections",
    "Errors and fixes",
    "Problem Solving",
    "All user messages",
    "Pending Tasks",
    "Current Work",
    "Optional Next Step",
];

/// A parsed compaction summary. Missing sections are simply absent — template
/// variants exist and a heading that never appeared is not an error.
#[derive(Debug, Clone, Default)]
pub struct Recap {
    pub sections: Vec<(String, String)>,
}

impl Recap {
    pub fn get(&self, heading: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(h, _)| h.eq_ignore_ascii_case(heading))
            .map(|(_, body)| body.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

/// Does this line open a section, and if so which one?
///
/// The template renders headings as `1. **Primary Request and Intent:**`, but
/// variants drop the bold, the number, or the colon, so match on the heading
/// text and tolerate the decoration around it.
fn heading_at(line: &str) -> Option<&'static str> {
    let t = line
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '#' || c == ' ')
        .trim_start_matches('*')
        .trim();
    HEADINGS.iter().copied().find(|h| {
        // `get` rather than direct slicing: these summaries are full of Turkish,
        // and a byte index that lands inside a multi-byte char panics. Returning
        // None there is exactly right — a line whose first h.len() bytes are not
        // a whole prefix cannot be this heading anyway.
        match (t.get(..h.len()), t.get(h.len()..)) {
            (Some(head), Some(rest)) => {
                head.eq_ignore_ascii_case(h)
                    // Reject prose that merely opens with the same words.
                    && rest.trim_start_matches(['*', ':', ' ']).is_empty()
            }
            _ => false,
        }
    })
}

/// Split a compaction summary into its sections.
pub fn parse(summary: &str) -> Recap {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;

    for line in summary.lines() {
        if let Some(h) = heading_at(line) {
            if let Some((name, body)) = current.take() {
                sections.push((name, body.join("\n").trim().to_string()));
            }
            current = Some((h.to_string(), Vec::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    if let Some((name, body)) = current {
        sections.push((name, body.join("\n").trim().to_string()));
    }
    Recap { sections }
}

/// What a session is about: its parsed summary, or failing that its opening ask.
#[derive(Debug, Clone)]
pub struct SessionRecap {
    pub recap: Recap,
    /// The first thing the user actually typed, always available.
    pub opening: Option<String>,
    /// How many compaction summaries the session has.
    pub compactions: usize,
}

/// Recover what a session was for.
///
/// Uses the newest summary: later compactions have seen more of the session, and
/// `Primary Request and Intent` is written cumulatively rather than being
/// overwritten, so the last one is the most complete account of the goal.
///
/// With no summary the honest answer is the opening message and nothing more.
/// Inferring "pending tasks" from raw turns is a different and much less reliable
/// problem — decision and action-item extraction is measurably weak — and a
/// confidently wrong list of open items is worse than none.
pub fn session_recap(store: &Store, session_pk: i64) -> Result<SessionRecap> {
    let summaries = store.compact_summaries(session_pk)?;
    let recap = summaries.last().map(|s| parse(s)).unwrap_or_default();
    Ok(SessionRecap {
        recap,
        opening: store.first_spoken_message(session_pk)?,
        compactions: summaries.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = r#"This session is being continued from a previous conversation that ran out of context.

Summary:
1. **Primary Request and Intent:**

   The user is building a local desktop version sharing one codebase.
   - "naptin lan local llm calistirma amk deepseek devam"

2. **Key Technical Concepts:**
   - Tauri, SQLite

7. **Pending Tasks:**
   - Build the query-reformulation fix (user's explicit "go")
   - Validate with bench-gate

8. **Current Work:**
   Editing llm.rs
"#;

    #[test]
    fn parses_the_real_template() {
        let r = parse(REAL);
        assert_eq!(r.sections.len(), 4, "{:?}", r.sections);
        assert!(r
            .get("Primary Request and Intent")
            .unwrap()
            .contains("local desktop version"));
        assert!(r.get("Pending Tasks").unwrap().contains("bench-gate"));
        assert!(r.get("Current Work").unwrap().contains("llm.rs"));
        // A heading the summary never used is absent, not empty.
        assert!(r.get("Optional Next Step").is_none());
    }

    #[test]
    fn preamble_before_the_first_heading_is_dropped() {
        let r = parse(REAL);
        let first = &r.sections[0].1;
        assert!(
            !first.contains("ran out of context"),
            "boilerplate leaked into the first section: {first}"
        );
    }

    #[test]
    fn tolerates_template_variants() {
        for line in [
            "1. **Pending Tasks:**",
            "**Pending Tasks:**",
            "## Pending Tasks",
            "7. Pending Tasks:",
            "Pending Tasks",
        ] {
            assert_eq!(
                heading_at(line),
                Some("Pending Tasks"),
                "failed on {line:?}"
            );
        }
    }

    /// Prose that merely opens with the same words must not start a section, or
    /// a summary that discusses its own structure would fragment.
    #[test]
    fn prose_mentioning_a_heading_is_not_a_heading() {
        assert_eq!(heading_at("Pending Tasks were not recorded here"), None);
        assert_eq!(
            heading_at("   - Current Work is the editing of llm.rs"),
            None
        );
    }

    /// Regression: these summaries are largely Turkish, and slicing a heading
    /// candidate by byte length panicked on the first real one tried.
    #[test]
    fn turkish_lines_do_not_panic() {
        for line in [
            "   - kullanıcı şunu istedi: çalıştır",
            "ığüşöç",
            "1. **Öncelikli İstek:**",
            "",
        ] {
            assert_eq!(heading_at(line), None, "{line:?}");
        }
        let r = parse("1. **Pending Tasks:**\n   - çalıştır ve doğrula amk\n");
        assert!(r.get("Pending Tasks").unwrap().contains("doğrula"));
    }

    #[test]
    fn empty_summary_parses_to_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("just some prose with no headings at all").is_empty());
    }
}
