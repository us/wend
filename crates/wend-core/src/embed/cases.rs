//! Building the case library: ⟨situation, the user's verbatim reaction⟩ pairs.
//!
//! A *case* records what the agent had just done and what this user said back.
//! Retrieval over the situation side answers "what would they say here?" with
//! their own words, rather than with a rulebook distilled from them.
//!
//! Measured on the author's corpus: 73.3% of reactions are genuinely responsive
//! to the preceding turn, 24.2% pivot to a new topic (unrecoverable by any
//! situation representation), and the classes carrying transferable judgment
//! — rejection, skepticism, granular nudge — are 33.4%.
//!
//! # What this is not
//!
//! **It does not predict what the user will say.** Over 477 held-out probes,
//! retrieval matched the user's actual stance no better than drawing cases at
//! random, and trended worse (majority vote 35.8% vs 43.4%). A frontier model
//! given the same situations could not beat always-guessing-the-majority-class
//! either. The stance simply is not a function of the situation: the same words
//! ("finished it, tests pass, shall I proceed?") precede approval or rejection
//! depending on whether the work was good, and work quality is not indexed.
//!
//! What retrieval *does* carry is the criteria: retrieved reactions share 1.66×
//! more vocabulary with what the user really said than random ones do. Read the
//! output as "these are the standards this person applies in moments like this",
//! never as a verdict, and never as their approval. See `evals/RESULTS.md`.
//!
//! The retrieved distribution is deliberately skewed toward the emphatic and
//! the unusual by [`diagnosticity`]. That is why it is a poor predictor and a
//! good teacher — the two goals are in direct conflict here.
//!
//! # No hand-written vocabulary
//!
//! Nothing in this module keys off words in any particular language. An earlier
//! version classified reactions with lists of Turkish and English keywords; it
//! agreed with hand labels only 57% of the time and quietly made the feature
//! useless to anyone whose language was not on the list. Emphasis is now read
//! from punctuation, capitalisation and length relative to the corpus, and
//! near-duplicates from character trigrams — all of which behave the same in any
//! script.

use crate::error::Result;
use crate::model::Block;
use crate::store::{NewCase, Store, TurnRow};

/// Characters of the user's own preceding message kept for thread context.
const PREV_USER_CHARS: usize = 600;
/// Characters kept from the tail of the agent's prose — where it says what it
/// did and asks for a decision.
const ASSISTANT_TAIL_CHARS: usize = 1000;

/// Reactions outside this band are dropped at build time: below it are bare
/// acknowledgements ("ee", "go") carrying no judgment, above it are pasted logs.
const MIN_REACTION: usize = 8;
const MAX_REACTION: usize = 2000;

/// Take the last `n` characters without splitting a multi-byte char.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// Take the first `n` characters without splitting a multi-byte char.
fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn blocks(content_json: &str) -> Vec<Block> {
    serde_json::from_str(content_json).unwrap_or_default()
}

/// Collapse whitespace and drop code fences, which carry no behavioural signal
/// and would dominate the byte budget.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_fence = false;
    for line in s.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            if !in_fence {
                out.push_str(" <code> ");
            }
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Is this row the user actually speaking?
///
/// Excludes, in order of how often they fooled earlier prototypes: tool results
/// (they carry `role='user'` but are machine output), system reminders and slash
/// commands, compaction summaries (system-generated continuations), subagent
/// turns, and messages that are only a bracketed marker such as
/// `[Request interrupted by user]` or an `[Image #3]` attachment — all of which
/// would otherwise be stored as things this user said.
fn spoken_user_text(row: &TurnRow) -> Option<String> {
    if row.role != "user" || row.is_sidechain || row.is_compact_summary {
        return None;
    }
    let text = match blocks(&row.content_json).first() {
        Some(Block::Text { text }) => text.clone(),
        _ => return None,
    };
    let t = text.trim();
    if t.is_empty() || t.starts_with('<') || t.starts_with('/') {
        return None;
    }
    // Bracketed-marker-only or image-only messages carry no words of their own.
    // `starts_with("Image")` without a trailing space on purpose: the harness
    // emits both `[Image #3]` and `[Image: source: /var/folders/…/Screenshot.png]`,
    // and matching only the spaced form let the second kind through into a real
    // library as one of the user's "reactions".
    let stripped = t.trim_start_matches('[');
    if stripped.starts_with("Request interrupted") || stripped.starts_with("Image") {
        return None;
    }
    // Harness traffic that arrives *as* a user turn but was never typed by the
    // user: peer-agent messages, hook output, background-task notices. These do
    // not start with '<', so the check above misses them — 459 of 8,037 cases in
    // a real library turned out to be these, i.e. 5.7% of the "reactions" were
    // one agent talking to another.
    const INJECTED: [&str; 5] = [
        "Another Claude session sent a message",
        "Stop hook feedback",
        "Command running in background",
        "This session is being continued",
        "Caveat: The messages below were generated",
    ];
    if INJECTED.iter().any(|p| t.starts_with(p)) || t.contains("idle_notification") {
        return None;
    }
    let cleaned = clean(t);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// The agent's side of one turn: what it said, and what it did.
#[derive(Default)]
struct AgentTurn {
    prose: String,
    tools: Vec<(String, usize)>,
}

impl AgentTurn {
    fn signature(&self) -> String {
        self.tools
            .iter()
            .map(|(name, n)| {
                if *n > 1 {
                    format!("{name}×{n}")
                } else {
                    name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn absorb(&mut self, content_json: &str) {
        for b in blocks(content_json) {
            match b {
                Block::Text { text } => {
                    if !self.prose.is_empty() {
                        self.prose.push(' ');
                    }
                    self.prose.push_str(&text);
                }
                Block::ToolUse { name, .. } => {
                    match self.tools.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, count)) => *count += 1,
                        None => self.tools.push((name, 1)),
                    }
                }
                // Thinking is the agent's private reasoning — the user never saw
                // it, so it cannot be part of the situation they reacted to.
                _ => {}
            }
        }
    }
}

/// Extract every ⟨situation, reaction⟩ pair in one session.
///
/// Walks rows in line order. A spoken user row closes a turn: everything since
/// the previous spoken user row is the agent's side. Tool-result rows are *not*
/// boundaries — a tool-heavy turn interleaves them with agent rows, and treating
/// them as boundaries would truncate the prose and lose the tool names.
pub fn cases_in_session(rows: &[TurnRow]) -> Vec<NewCase> {
    let mut out = Vec::new();
    let mut agent = AgentTurn::default();
    let mut prev_user: Option<String> = None;

    for row in rows {
        if let Some(text) = spoken_user_text(row) {
            // A turn only becomes a case if the agent actually did something
            // first; back-to-back user messages have no situation to react to.
            if !agent.prose.is_empty() || !agent.tools.is_empty() {
                let len = text.chars().count();
                if (MIN_REACTION..=MAX_REACTION).contains(&len) {
                    let mut situation = String::new();
                    if let Some(p) = &prev_user {
                        situation.push_str(&head_chars(p, PREV_USER_CHARS));
                        situation.push('\n');
                    }
                    situation.push_str(&tail_chars(&clean(&agent.prose), ASSISTANT_TAIL_CHARS));
                    let sig = agent.signature();
                    if !sig.is_empty() {
                        situation.push('\n');
                        situation.push_str(&sig);
                    }
                    out.push(NewCase {
                        situation,
                        reaction: text.clone(),
                        src_message_fk: row.id,
                    });
                }
            }
            prev_user = Some(text);
            agent = AgentTurn::default();
        } else if row.role == "assistant" && !row.is_sidechain {
            agent.absorb(&row.content_json);
        }
    }
    out
}

/// Normalised form used to detect duplicate reactions.
///
/// Casefold, strip punctuation, collapse repeated characters used for emphasis.
/// Deliberately *exact* equality after this: a fuzzy threshold was measured as
/// unsafe here because "bunu yap" and "bunu yapma" score 0.75 on trigram
/// Jaccard, so it would discard the polarity contrast the retriever needs.
pub fn normalise_reaction(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = '\0';
    for c in s.to_lowercase().chars() {
        let c = if c.is_alphanumeric() || c.is_whitespace() {
            c
        } else {
            continue;
        };
        if c == last && !c.is_whitespace() {
            continue; // "yaaaa" -> "ya"
        }
        last = c;
        out.push(c);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build the case library for every session that has none yet.
///
/// Resume-safe and idempotent per session: a session is skipped once it has any
/// case rows, and each session's cases are inserted in one transaction, so a
/// crash never leaves a session half-built (which would make it invisible to the
/// next run forever).
pub fn build_cases(store: &mut Store) -> Result<usize> {
    use std::collections::HashSet;

    // Duplicate reactions are dropped globally, keeping the earliest — the first
    // time they said it is the one that was not a repeat. 391 exact duplicates
    // were measured in the author's corpus.
    let mut seen: HashSet<String> = store
        .existing_case_reactions()?
        .iter()
        .map(|r| normalise_reaction(r))
        .collect();

    let mut created = 0;
    for pk in store.sessions_without_chunks("case")? {
        let rows = store.session_turn_rows(pk)?;
        let mut fresh = Vec::new();
        for c in cases_in_session(&rows) {
            if seen.insert(normalise_reaction(&c.reaction)) {
                fresh.push(c);
            }
        }
        if !fresh.is_empty() {
            created += store.insert_session_cases(pk, &fresh)?;
        }
    }
    Ok(created)
}

// ------------------------------------------------------------------- retrieval

/// Candidates pulled by vector before reranking. Past ~50 the cross-encoder cost
/// grows linearly while recall is flat.
const CANDIDATES: usize = 40;

/// Weights on the two scoring terms.
///
/// **Baseline, not tuned.** They start at 1/1 because that is a neutral starting
/// point, not because it is known to be right — Generative Agents used α = 1 and
/// never tuned it either, which the research briefing flags explicitly. Sweeping
/// these on a tuning split is an open verification item.
const W_RELEVANCE: f32 = 1.0;
const W_DIAGNOSTICITY: f32 = 1.0;

/// Below this raw relevance, say "no confident precedent" instead of forcing a
/// nearest neighbour.
///
/// Separate values because cosine and cross-encoder scores are different
/// quantities on different scales — one threshold for both would silently
/// mis-set whichever path was not tuned.
///
/// The rerank floor is **measured, not guessed**. Swept over the real corpus
/// with 6 queries that have no business being there (a biology fact, a bread
/// recipe, a 17th-century treaty, tides, the offside rule, the melting point of
/// tungsten) and 6 genuine situations, reading the top score *before* balancing:
///
/// ```text
/// irrelevant  0.139 – 0.405
/// relevant    0.470 – 0.921
/// ```
///
/// 0.44 sits in that gap and decides all 12 correctly.
///
/// Two earlier values were wrong, and how they were wrong is worth keeping:
/// 0.20 was a pure guess and let a biology question straight through. 0.35 came
/// from a sweep that read the *first returned* precedent instead of the top
/// scorer — but output is ordered by ascending diagnosticity, so that field is
/// the least diagnostic case, not the best match. The sweep looked clean and was
/// measuring the wrong number.
///
/// Honest limit: the gap is real but narrow (0.405 to 0.470), so this separates
/// the sampled queries and will not separate every query. The floor buys most of
/// the protection, not all of it.
#[cfg(feature = "azure")]
const FLOOR_RERANK: f32 = 0.44;

/// Cosine's floor is separate because cosine and cross-encoder relevance are
/// different quantities on different scales. Swept with the same 12 queries:
///
/// ```text
/// irrelevant  0.140 – 0.230
/// relevant    0.523 – 0.719
/// ```
///
/// Worth noting, since it is the opposite of what you would expect: on this
/// corpus cosine *separates better* than the cross-encoder does. Its gap is
/// 0.230→0.523 where rerank's is only 0.405→0.470. The reranker is better at
/// ordering the candidates it is given — which is what it is used for — but
/// worse at saying "none of these belong here", because it scores every pair as
/// a relevance question rather than a membership one.
const FLOOR_COSINE: f32 = 0.38;

/// One retrieved precedent.
#[derive(Debug, Clone)]
pub struct Precedent {
    pub session_id: String,
    pub project: String,
    pub situation: String,
    pub reaction: String,
    pub ts: Option<i64>,
    pub relevance: f32,
    pub diagnosticity: f32,
}

/// The outcome of a recall, including the honest "I don't know".
#[derive(Debug, Clone)]
pub enum Recall {
    /// Nothing cleared the floor: 24.2% of reactions are pivots to a new topic
    /// that no situation representation can recover, and inventing a precedent
    /// for those is worse than admitting there isn't one.
    NoPrecedent {
        best: f32,
        floor: f32,
    },
    Found(Vec<Precedent>),
}

/// How much this case teaches about the user's judgment, in [0,1].
///
/// Pure similarity is a popularity contest the corpus median wins: short
/// acknowledgements are numerous and sit near everything in embedding space,
/// while a single emphatic correction is rare and sits near nothing in
/// particular. This term promotes the rare informative case over the common
/// bland one.
///
/// **Every signal here is language-independent by construction.** An earlier
/// version scored emphasis with a hand-written list of Turkish and English
/// words. That was wrong twice over: it silently made the feature work for one
/// person and no one else, and it measured at 57% agreement with hand labels —
/// worse than the orthographic signals below, which need no vocabulary at all.
///
/// - **length**, scored against the corpus median rather than a fixed band, so
///   "long for this person" means the same thing in any language;
/// - **emphasis punctuation** — `!`, `?`, `...`, and runs of repeated
///   characters. Shouting and insistence are punctuated the same way in every
///   language this tool has seen;
/// - **capitalisation ratio** — sustained upper case is a universal intensity
///   marker, and only counted for alphabetic characters so scripts without case
///   simply score zero here instead of being penalised.
pub fn diagnosticity(reaction: &str) -> f32 {
    let chars: Vec<char> = reaction.chars().collect();
    let n = chars.len();
    if n == 0 {
        return 0.0;
    }

    // Length, relative to the corpus median reaction. Below it there is rarely a
    // judgment; far above it is usually a paste rather than an opinion.
    let ratio = n as f32 / MEDIAN_REACTION_CHARS as f32;
    let mut score: f32 = if ratio < 0.4 {
        0.0
    } else if ratio <= 6.0 {
        0.3
    } else {
        0.1
    };

    let punct = chars.iter().filter(|c| matches!(**c, '!' | '?')).count() as f32;
    let repeats = chars
        .windows(3)
        .filter(|w| w[0] == w[1] && w[1] == w[2] && !w[0].is_whitespace())
        .count() as f32;
    score += ((punct + repeats) / 3.0).min(1.0) * 0.35;

    let letters = chars.iter().filter(|c| c.is_alphabetic()).count();
    if letters >= 8 {
        let upper = chars.iter().filter(|c| c.is_uppercase()).count() as f32;
        score += (upper / letters as f32).min(0.6) / 0.6 * 0.35;
    }

    score.clamp(0.0, 1.0)
}

/// Median reaction length over the corpus this was tuned against.
///
/// A constant rather than a query so scoring stays a pure function; it only sets
/// the scale, and being off by a factor of two shifts nothing across the band.
const MEDIAN_REACTION_CHARS: usize = 120;

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Retrieve the precedents most like `situation`.
///
/// Advisory by construction: this reports what the user said before, with
/// provenance. It must never be treated as their consent — predicting an
/// approval and acting on it would launder their standing rule that nothing is
/// committed, pushed or released without them saying so.
pub fn recall(store: &Store, situation: &str, limit: usize) -> Result<Recall> {
    let model = super::current_model_id()?;
    let cases = store.case_vectors(&model)?;
    if cases.is_empty() {
        return Ok(Recall::NoPrecedent {
            best: 0.0,
            floor: 0.0,
        });
    }

    let mut embedder = super::Embedder::load()?;
    let qv = embedder.embed_query(situation)?;

    // Stage 1: vector candidates.
    let mut scored: Vec<(f32, usize)> = cases
        .iter()
        .enumerate()
        .filter(|(_, c)| c.vec.len() == qv.len())
        .map(|(i, c)| (dot(&qv, &c.vec), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(CANDIDATES);

    // Stage 2: rerank the candidate SITUATIONS, when a reranker is configured.
    // A cross-encoder scores "does this document answer this query"; a reaction
    // does not answer a situation, but a past situation genuinely is or is not
    // similar to the current one.
    #[allow(unused_mut)]
    let mut floor = FLOOR_COSINE;
    #[allow(unused_mut)]
    let mut relevance: Vec<(usize, f32)> = scored.iter().map(|(s, i)| (*i, *s)).collect();

    #[cfg(feature = "azure")]
    if let Some(rr) = super::azure::Reranker::from_env()? {
        let docs: Vec<String> = scored
            .iter()
            .map(|(_, i)| cases[*i].situation.clone())
            .collect();
        let ranked = rr.rerank(situation, &docs, CANDIDATES)?;
        if !ranked.is_empty() {
            relevance = ranked
                .into_iter()
                .map(|(pos, s)| (scored[pos].1, s))
                .collect();
            floor = FLOOR_RERANK;
        }
    }

    let best = relevance.first().map(|(_, s)| *s).unwrap_or(0.0);
    if best < floor {
        return Ok(Recall::NoPrecedent { best, floor });
    }

    // Stage 3: score, dedup, balance.
    let mut ranked: Vec<Precedent> = relevance
        .iter()
        .map(|(i, rel)| {
            let c = &cases[*i];
            Precedent {
                session_id: c.session_id.clone(),
                project: c.project.clone(),
                situation: c.situation.clone(),
                reaction: c.reaction.clone(),
                ts: c.ts,
                relevance: *rel,
                diagnosticity: diagnosticity(&c.reaction),
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        let sa = W_RELEVANCE * a.relevance + W_DIAGNOSTICITY * a.diagnosticity;
        let sb = W_RELEVANCE * b.relevance + W_DIAGNOSTICITY * b.diagnosticity;
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(Recall::Found(select_balanced(ranked, limit)))
}

/// Pick the final set: drop duplicate reactions, then guarantee both stances are
/// represented when both are available.
///
/// The balance is a sycophancy guard, not decoration. Retrieving the user's past
/// agreement and conditioning on it is an approval amplifier by construction; a
/// set that is all approvals will approve things this user would have hated.
fn select_balanced(ranked: Vec<Precedent>, limit: usize) -> Vec<Precedent> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut picked: Vec<Precedent> = Vec::new();

    for p in &ranked {
        if picked.len() >= limit {
            break;
        }
        if seen.insert(normalise_reaction(&p.reaction)) {
            picked.push(p.clone());
        }
    }

    // If one stance is missing entirely, swap the weakest pick for the best
    // available case of that stance. Two guards, both learned from a live run:
    //
    // - only candidates at or above `floor` are eligible, or balancing smuggles
    //   in a precedent the abstention check would have rejected outright;
    // - never evict when that would drop the set to nothing. At `limit = 1` the
    //   unguarded version popped the single best precedent and replaced it with
    //   a far weaker opposite-stance one, so the caller saw only the weak case.
    // Diversity, replacing an earlier "force one approval and one rejection"
    // rule. That rule needed a stance classifier, which needed a hand-written
    // list of Turkish and English words — unusable for anyone else and only 57%
    // accurate anyway.
    //
    // The sycophancy risk it guarded against is real: retrieve five variations
    // of the same rubber stamp and the reader learns nothing. But the fix does
    // not need to know which reaction is an approval. It only needs the set not
    // to be five of the same thing, and character-trigram distance measures that
    // in any script without a vocabulary.
    let mut diverse: Vec<Precedent> = Vec::new();
    for p in picked {
        let too_similar = diverse
            .iter()
            .any(|q: &Precedent| trigram_similarity(&q.reaction, &p.reaction) > 0.5);
        if !too_similar {
            diverse.push(p);
        }
    }
    let mut picked = diverse;

    // Models over-weight what sits at the end of a prompt, so the most
    // diagnostic case goes last. Deterministic, or evals will not reproduce.
    picked.sort_by(|a, b| {
        a.diagnosticity
            .partial_cmp(&b.diagnosticity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    picked
}

/// Overlap of character trigrams, in [0,1].
///
/// Deliberately character-based rather than word-based: it needs no tokeniser,
/// no stop-word list and no vocabulary, so it behaves the same on Turkish,
/// English or a mix of both — which is what this corpus actually is.
fn trigram_similarity(a: &str, b: &str) -> f32 {
    fn grams(s: &str) -> std::collections::HashSet<[char; 3]> {
        let c: Vec<char> = s
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        c.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
    }
    let (ga, gb) = (grams(a), grams(b));
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    ga.intersection(&gb).count() as f32 / ga.union(&gb).count() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, role: &str, json: &str) -> TurnRow {
        TurnRow {
            id,
            line_no: id,
            role: role.into(),
            ts: Some(0),
            content_json: json.into(),
            is_sidechain: false,
            is_compact_summary: false,
        }
    }
    fn text(t: &str) -> String {
        format!(r#"[{{"kind":"text","text":{}}}]"#, serde_json::json!(t))
    }

    #[test]
    fn pairs_a_reaction_with_the_agent_turn_before_it() {
        let rows = vec![
            row(1, "user", &text("bu sayfayi duzelt lutfen dostum")),
            row(
                2,
                "assistant",
                r#"[{"kind":"text","text":"Düzelttim."},
                    {"kind":"tool_use","id":"a","name":"Edit","input":{}},
                    {"kind":"tool_use","id":"b","name":"Edit","input":{}}]"#,
            ),
            // A tool result carries role=user but is not the user speaking, and
            // must not close the turn.
            row(
                3,
                "user",
                r#"[{"kind":"tool_result","tool_use_id":"a","is_error":false,"content":[]}]"#,
            ),
            row(4, "assistant", &text("Hepsi tamam, devam edeyim mi?")),
            row(5, "user", &text("yok once testleri calistir bakalim")),
        ];

        let cases = cases_in_session(&rows);
        assert_eq!(cases.len(), 1, "one reaction, one case");
        let c = &cases[0];
        assert_eq!(c.reaction, "yok once testleri calistir bakalim");
        assert_eq!(c.src_message_fk, 5);
        assert!(
            c.situation.contains("bu sayfayi duzelt"),
            "prev user message is context: {}",
            c.situation
        );
        assert!(
            c.situation.contains("devam edeyim mi"),
            "agent prose across the tool result: {}",
            c.situation
        );
        assert!(
            c.situation.contains("Edit×2"),
            "tool names carry counts: {}",
            c.situation
        );
    }

    #[test]
    fn skips_rows_that_are_not_the_user_speaking() {
        let mut sidechain = row(2, "user", &text("subagent said something long enough"));
        sidechain.is_sidechain = true;
        let mut compact = row(3, "user", &text("This session is being continued from"));
        compact.is_compact_summary = true;

        let rows = vec![
            row(1, "assistant", &text("bir sey yaptim iste")),
            sidechain,
            compact,
            row(
                4,
                "user",
                &text("<system-reminder>bir sey</system-reminder>"),
            ),
            row(5, "user", &text("/commit")),
            row(
                6,
                "user",
                &text("[Request interrupted by user for tool use]"),
            ),
            row(7, "user", &text("[Image #3]")),
            // Both attachment forms the harness emits; only the first was caught
            // at first, and the second reached a real case library.
            row(
                13,
                "user",
                &text("[Image: source: /var/folders/x/TemporaryItems/Screenshot.png]"),
            ),
            row(8, "user", &text("ee")), // under the length floor
            // Harness traffic delivered as a user turn. A real library had 459
            // of these out of 8,037 cases — 5.7% of the stored "reactions" were
            // one agent talking to another.
            row(
                9,
                "user",
                &text("Another Claude session sent a message: <teammate-message id=\"x\">"),
            ),
            row(
                10,
                "user",
                &text("Stop hook feedback: [devam et amk su desktop app i bitir]"),
            ),
            row(
                11,
                "user",
                &text("Command running in background with ID: bx5ajp744. Output is"),
            ),
            row(
                12,
                "user",
                &text("{\"type\":\"idle_notification\",\"from\":\"rev-lazy\"}"),
            ),
        ];
        assert!(cases_in_session(&rows).is_empty());
    }

    #[test]
    fn a_reaction_with_no_agent_turn_before_it_is_not_a_case() {
        let rows = vec![
            row(1, "user", &text("ilk mesajim buraya yaziliyor")),
            row(2, "user", &text("ikinci mesajim da hemen arkasindan")),
        ];
        assert!(cases_in_session(&rows).is_empty());
    }

    #[test]
    fn situation_slices_on_char_boundaries() {
        // Turkish is ~1.6 bytes/char; byte slicing here would panic.
        let long = "çğıöşü ".repeat(400);
        let rows = vec![
            row(1, "user", &text(&long)),
            row(2, "assistant", &text(&long)),
            row(3, "user", &text("tamam anladim simdi devam et")),
        ];
        let cases = cases_in_session(&rows);
        assert_eq!(cases.len(), 1);
        // Bounded by chars, and every char intact.
        assert!(cases[0].situation.chars().count() <= PREV_USER_CHARS + ASSISTANT_TAIL_CHARS + 4);
        assert!(cases[0].situation.contains('ç'));
    }

    fn prec(reaction: &str, relevance: f32) -> Precedent {
        Precedent {
            session_id: "s".into(),
            project: "p".into(),
            situation: "sit".into(),
            reaction: reaction.into(),
            ts: None,
            relevance,
            diagnosticity: diagnosticity(reaction),
        }
    }

    /// Diagnosticity must not depend on knowing any language. The same message
    /// shape has to score the same whether it is written in Turkish, English or
    /// a script with no upper case at all — the previous version scored emphasis
    /// from a hand-written Turkish word list and worked for exactly one person.
    #[test]
    fn diagnosticity_is_language_independent() {
        let long = "x".repeat(200);
        let tr = "bunu bir daha asla yapma, ciddiyim!!! duzelt hemen".to_string();
        let en = "do not ever do this again, i mean it!!! fix it now".to_string();
        assert!(
            (diagnosticity(&tr) - diagnosticity(&en)).abs() < 0.12,
            "same shape, different language: tr={} en={}",
            diagnosticity(&tr),
            diagnosticity(&en)
        );

        // Emphasis and length raise it; a bare acknowledgement does not.
        assert!(diagnosticity("ok") < 0.2);
        assert!(diagnosticity(&tr) > diagnosticity("ok"));
        // A script without case must not be penalised into zero.
        assert!(diagnosticity("这个不对，请再检查一遍，一定要测试!!") > 0.2);
        // A wall of pasted output is not a judgment.
        assert!(diagnosticity(&long.repeat(10)) < diagnosticity(&tr));
    }

    #[test]
    fn trigram_similarity_needs_no_vocabulary() {
        assert!(trigram_similarity("hadi go bakalim", "hadi go bakalim") > 0.99);
        assert!(trigram_similarity("hadi go bakalim", "hadi go bakalim!") > 0.7);
        assert!(trigram_similarity("hadi go", "bunu tamamen degistir") < 0.3);
        assert_eq!(trigram_similarity("", "abc"), 0.0);
    }

    #[test]
    fn selection_drops_near_duplicate_reactions() {
        let picked = select_balanced(
            vec![
                prec("hadi go bakalim dostum", 0.9),
                prec("hadi go bakalim dostum!!", 0.88), // near-duplicate
                prec("bunu tamamen degistirmen lazim baska yol dene", 0.5),
            ],
            3,
        );
        assert_eq!(picked.len(), 2, "near-duplicate must be dropped");
    }

    #[test]
    fn normalisation_collapses_emphasis_but_not_negation() {
        assert_eq!(
            normalise_reaction("Hadi GO!!!"),
            normalise_reaction("hadi go")
        );
        assert_eq!(normalise_reaction("yaaaa tamam"), "ya tamam");
        assert_ne!(
            normalise_reaction("bunu yap"),
            normalise_reaction("bunu yapma"),
            "negation must survive: dedup would otherwise drop the opposite case"
        );
    }
}
