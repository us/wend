// profile-mine — multi-agent mining of the user's durable standing preferences.
//
// Reads EVERY message (not a sample): the corpus is sharded so each shard is
// read IN FULL by one agent. Language-agnostic — agents infer the user's own
// language(s) from the corpus; nothing about any specific language is hardcoded.
//
// Inputs (produced by build_sample.py before this runs):
//   /tmp/wend-profile.json          full corpus [{session_id,project,title,ts,line_no,text}]
//   /tmp/prof/shard_00.txt .. NN     every message, in conversation order, ~400KB each
// Pass the shard count as args.shards (build_sample.py prints it).
//
// Flow: read-all shards -> candidate rules -> consolidate/dedup -> adversarially
// verify each against the FULL corpus (distinct sessions & projects) -> keep only
// broad ones -> synthesize a tight, ordered CLAUDE.md block.
// Returns { kept_count, kept, block }.

export const meta = {
  name: 'profile-mine',
  description: "Read every message in the user's Claude Code history (sharded), mine durable standing preferences, verify prevalence adversarially, synthesize a tight CLAUDE.md block",
  phases: [
    { title: 'Read', detail: 'one agent per shard reads it in full and extracts candidate standing rules' },
    { title: 'Consolidate', detail: 'merge + dedup across shards into one canonical candidate list' },
    { title: 'Verify', detail: 'quantify each candidate across the full corpus; keep broad ones (+ rescue cross-project)' },
    { title: 'Insights', detail: 'second pass per shard: context / friction / goals — not commands' },
    { title: 'Curate', detail: 'dedup insights into the useful, well-evidenced ones' },
    { title: 'Synthesize', detail: 'emit two blocks: standing defaults + context/watch-outs' },
  ],
}

const CORPUS = '/tmp/wend-profile.json'
// args may arrive as a parsed object OR a JSON string — handle both.
const A = typeof args === 'string' ? JSON.parse(args) : (args || {})
const N = A.shards || 4
const pad = (i) => String(i).padStart(2, '0')

const DIMENSIONS =
  `- Workflow order: the sequence of steps the user drives a task through, start to ship (investigate, plan, review, implement, PR, merge, deploy, test) — and in what order.\n` +
  `- Explicit recurring requests: concrete things they ask for again and again (e.g. investigate before acting, give a recommendation not options, produce N variants, use isolated branches, run review loops).\n` +
  `- Explicit recurring prohibitions: things they repeatedly say NOT to do or that aren't needed; scope/output they want cut.\n` +
  `- Delivery & verification: autonomy expectations, taking work end-to-end, how/where they want changes tested (real environment vs. only compile/tests), architecture biases.\n` +
  `- Decision & collaboration style: when they want a recommendation vs. options, when they want to understand/discuss rather than get code, what they want proactively.`

const FIND_SCHEMA = {
  type: 'object',
  required: ['rules', 'user_language'],
  properties: {
    user_language: { type: 'string', description: "The language(s) the user writes in, as observed in this shard (e.g. 'English', 'Turkish + English')." },
    rules: {
      type: 'array',
      items: {
        type: 'object',
        required: ['rule', 'evidence', 'est_prevalence'],
        properties: {
          rule: { type: 'string', description: 'One concrete, actionable standing instruction for Claude ("do X" / "never Y").' },
          evidence: { type: 'array', items: { type: 'string' }, description: '2-3 short verbatim quote fragments from this shard' },
          est_prevalence: { type: 'string', enum: ['near-universal', 'common', 'occasional'] },
        },
      },
    },
  },
}

phase('Read')
const found = await parallel(Array.from({ length: N }, (_, i) => () =>
  agent(
    `You are mining a heavy Claude Code user's own past messages to extract their DURABLE standing preferences — things true across nearly all their work, so they never have to repeat them.\n\n` +
    `Read the ENTIRE shard file /tmp/prof/shard_${pad(i)}.txt — every line, paginating with Read offset/limit until the end. Do NOT skim or stop early. It is in conversation order with "--- session [project] ---" boundaries, so you can see how tasks unfold and judge recurrence across sessions and projects.\n\n` +
    `Look across ALL of these dimensions:\n${DIMENSIONS}\n\n` +
    `Rules:\n` +
    `- Return only CONCRETE, ACTIONABLE instructions. NO observations about the user's tone, mood, the way they address you, or filler/continuation phrases — those are useless.\n` +
    `- A rule qualifies only if it recurs across MANY sessions/projects in this shard, not once.\n` +
    `- Work in whatever language the user writes in; quote their own words as evidence.\n` +
    `- Also report which language(s) the user writes in.`,
    { label: `read:shard-${i}`, phase: 'Read', agentType: 'general-purpose', schema: FIND_SCHEMA }
  ).then(r => ({ shard: i, rules: (r && r.rules) || [], lang: r && r.user_language }))
))

const allRules = found.filter(Boolean).flatMap(f => f.rules)
const langs = [...new Set(found.filter(Boolean).map(f => f.lang).filter(Boolean))].join('; ')

const CONSOLIDATE_SCHEMA = {
  type: 'object',
  required: ['candidates'],
  properties: {
    candidates: {
      type: 'array',
      description: 'Deduped canonical rules, most-clearly-recurring first, max 16.',
      items: {
        type: 'object',
        required: ['rule', 'evidence'],
        properties: {
          rule: { type: 'string' },
          evidence: { type: 'array', items: { type: 'string' } },
        },
      },
    },
  },
}

phase('Consolidate')
const consolidated = await agent(
  `Candidate standing-rules were extracted by ${N} agents that each read a different slice of the same user's full history (JSON):\n\n${JSON.stringify(allRules, null, 1)}\n\n` +
  `Merge duplicates/overlaps into ONE canonical list of distinct, concrete, actionable rules (max 16), most-clearly-recurring first. Drop anything that is a tone/communication-style observation rather than an actionable instruction. Keep the best 2-3 verbatim evidence fragments per merged rule.`,
  { label: 'consolidate', phase: 'Consolidate', agentType: 'general-purpose', schema: CONSOLIDATE_SCHEMA }
)
const candidates = (consolidated && consolidated.candidates) || []

const VERIFY_SCHEMA = {
  type: 'object',
  required: ['rule', 'session_hits', 'projects_seen', 'prevalence', 'keep'],
  properties: {
    rule: { type: 'string', description: 'The rule, tightened/clarified if needed' },
    session_hits: { type: 'integer', description: 'distinct sessions whose messages show this pattern' },
    projects_seen: { type: 'integer', description: 'distinct projects where it appears' },
    prevalence: { type: 'string', enum: ['near-universal', 'common', 'occasional', 'rare'] },
    keep: { type: 'boolean', description: 'true ONLY if it recurs broadly (many sessions AND multiple projects)' },
    note: { type: 'string' },
  },
}

phase('Verify')
const verified = await parallel(candidates.map(c => () =>
  agent(
    `Adversarially verify whether this is really a NEAR-UNIVERSAL standing preference of the user, or just cherry-picked.\n\n` +
    `RULE: ${c.rule}\nEvidence claimed: ${JSON.stringify(c.evidence)}\n\n` +
    `The full corpus is ${CORPUS} — a JSON array of the user's own messages with session_id and project. Steps with python:\n` +
    `1. Compute denominators: total distinct sessions and projects.\n` +
    `2. Learn the user's own vocabulary: skim a sample of the messages to see the actual words/phrases they use for this behavior — DO NOT assume any particular language; derive your search terms from what THEY write.\n` +
    `3. Build 1-4 case-insensitive regexes capturing this rule's INTENT in the user's own language(s), and count how many DISTINCT sessions and DISTINCT projects contain a matching message.\n` +
    `4. Exclude compaction-summary pseudo-messages that merely echo a prior instruction.\n` +
    `Be skeptical: default keep=false; set keep=true only if it appears in many sessions AND across multiple projects. Report the real numbers even if they kill the rule.`,
    { label: `verify:${c.rule.slice(0, 32)}`, phase: 'Verify', agentType: 'general-purpose', schema: VERIFY_SCHEMA }
  )
))

const kept = verified.filter(Boolean).filter(v => v.keep).sort((a, b) => b.session_hits - a.session_hits)
// Rescue tier-2: real rules that span many PROJECTS even if the per-session
// count is lower (broadly applicable, just not restated every session).
const rescued = verified
  .filter(Boolean)
  .filter(v => !v.keep && (v.projects_seen >= 6 || v.session_hits >= 25))
  .sort((a, b) => b.projects_seen - a.projects_seen)
const keptAll = [...kept, ...rescued]

phase('Synthesize')
const block = await agent(
  `Write the final personal-preferences block for the user's ~/.claude/CLAUDE.md.\n\n` +
  `The user writes in: ${langs || 'their own language'}.\n\n` +
  `Use ONLY these verified, broadly-recurring rules (with their real prevalence numbers):\n${JSON.stringify(keptAll, null, 1)}\n\n` +
  `HARD CONSTRAINTS:\n` +
  `- Concrete, actionable standing instructions ONLY. NO observations about the user's tone, mood, the way they address you, or filler phrases — those are explicitly unwanted.\n` +
  `- These are things they want done by default so they never have to ask again.\n` +
  `- Order them the way the user's actual workflow flows (understand -> decide -> plan/review loop -> implement -> ship/verify), then cross-cutting don'ts.\n` +
  `- Do NOT hardcode secrets, tokens, or specific server IPs — generalize them.\n` +
  `- Write the bullets in the user's primary working language (${langs || 'infer from their evidence'}). Tight, one instruction per bullet, no preamble/outro.\n\n` +
  `Output EXACTLY this shape, filling the bullets:\n` +
  `<!-- wend:profile:start -->\n## My standing defaults (do these without asking)\n- ...\n<!-- wend:profile:end -->`,
  { label: 'synthesize', phase: 'Synthesize', agentType: 'general-purpose', effort: 'high' }
)

// ---- Track 2: qualitative insights (context / friction / goals) ----
// Not repeated COMMANDS — facts about the user and their work that would help an
// assistant serve them better if it knew them up front. No frequency-kill: a
// strong, well-evidenced fact is worth stating once.

const INSIGHT_SCHEMA = {
  type: 'object',
  required: ['items'],
  properties: {
    items: {
      type: 'array',
      items: {
        type: 'object',
        required: ['category', 'insight', 'evidence'],
        properties: {
          category: { type: 'string', enum: ['context', 'friction', 'goal', 'anti-pattern'] },
          insight: { type: 'string', description: 'One useful, reusable fact about the user or their work — NOT a command.' },
          evidence: { type: 'array', items: { type: 'string' }, description: '2-3 verbatim fragments' },
        },
      },
    },
  },
}

phase('Insights')
const insightsRaw = await parallel(Array.from({ length: N }, (_, i) => () =>
  agent(
    `Read the ENTIRE shard /tmp/prof/shard_${pad(i)}.txt (paginate to the end). This pass is NOT about repeated commands — capture what would help an assistant serve THIS user better if it knew it up front:\n` +
    `- context: who they are, what they build, their tech stack, tools, environments, the projects they work on.\n` +
    `- friction: recurring pain, rework, misunderstandings, or time-wasters — where the assistant repeatedly missed the mark or the user had to correct/redo things.\n` +
    `- goal: what they're actually trying to achieve (business goals, quality bars like beating a specific competitor / being SOTA, shipping fast).\n` +
    `- anti-pattern: things the assistant does that they push back on.\n\n` +
    `Prefer specific, reusable facts over generic ones; quote their own words as evidence. Work in whatever language they write in.`,
    { label: `insight:shard-${i}`, phase: 'Insights', agentType: 'general-purpose', schema: INSIGHT_SCHEMA }
  ).then(r => (r && r.items) || [])
))
const allInsights = insightsRaw.filter(Boolean).flat()

const CURATE_SCHEMA = {
  type: 'object',
  required: ['insights'],
  properties: {
    insights: {
      type: 'array',
      description: 'Deduped, well-evidenced, genuinely useful insights. Max 20.',
      items: {
        type: 'object',
        required: ['category', 'insight'],
        properties: {
          category: { type: 'string' },
          insight: { type: 'string' },
        },
      },
    },
  },
}

phase('Curate')
const curated = await agent(
  `Insights extracted by ${N} agents from a user's full history (JSON):\n\n${JSON.stringify(allInsights, null, 1)}\n\n` +
  `Merge duplicates, drop one-off/weakly-evidenced noise, and keep the distinct, genuinely useful facts (max 20), grouped by category (context, friction, goal, anti-pattern). Each must be concrete and reusable — something that would measurably help an assistant work with this user. No tone/filler observations.`,
  { label: 'curate', phase: 'Curate', agentType: 'general-purpose', schema: CURATE_SCHEMA }
)
const insights = (curated && curated.insights) || []

phase('Synthesize')
const context_block = await agent(
  `Write a SECOND block for the user's ~/.claude/CLAUDE.md giving an assistant useful CONTEXT and WATCH-OUTS about this user (distinct from the standing commands). Use ONLY these curated insights:\n${JSON.stringify(insights, null, 1)}\n\n` +
  `Constraints:\n` +
  `- Concrete, reusable facts and friction-preemptions ("I build X with stack Y"; "I often hit/worry about Z — so do W").\n` +
  `- Group loosely: who I am & what I build / how I like to work / watch out for.\n` +
  `- Do NOT hardcode secrets, tokens, or specific server IPs — generalize them.\n` +
  `- Write in the user's language (${langs || 'infer from evidence'}). Tight bullets, no preamble/outro.\n\n` +
  `Output EXACTLY this shape:\n` +
  `<!-- wend:context:start -->\n## Context about me & what to watch for\n- ...\n<!-- wend:context:end -->`,
  { label: 'context-synth', phase: 'Synthesize', agentType: 'general-purpose', effort: 'high' }
)

return { kept_count: keptAll.length, kept: keptAll, block, insights, context_block }
