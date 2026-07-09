// skill-forge — multi-agent generation of a topic skill from the user's history.
//
// Reads EVERY message from the topic-related sessions (sharded, each shard read
// IN FULL by one agent), consolidates what the user wants / rejects / how they
// work on this topic, then writes a complete SKILL.md. Language-agnostic.
//
// Inputs (from collect_topic.py):  /tmp/topic/shard_00.txt .. NN
// args: { shards: N, topic: "...", skillName: "kebab-name" }
// Returns { skill_md, findings }.

export const meta = {
  name: 'skill-forge',
  description: "Read every message from a topic's related sessions (sharded), distill what the user wants/rejects, and write a complete topic skill",
  phases: [
    { title: 'Read', detail: 'one agent per shard reads it in full and extracts wants / hows / rejections / workflow / examples' },
    { title: 'Consolidate', detail: 'merge findings across shards into one deduped set' },
    { title: 'Write', detail: 'synthesize the final SKILL.md, grounded in the user\'s own words' },
  ],
}

// args may arrive as a parsed object OR a JSON string — handle both, else the
// shard count / topic / name silently fall back to defaults (and shards go unread).
const A = typeof args === 'string' ? JSON.parse(args) : (args || {})
const N = A.shards || 3
const TOPIC = A.topic || 'this topic'
const SKILL_NAME = A.skillName || 'generated-skill'
const pad = (i) => String(i).padStart(2, '0')

const FIND_SCHEMA = {
  type: 'object',
  required: ['wants', 'hows', 'rejections', 'workflow', 'examples'],
  properties: {
    wants: { type: 'array', items: { type: 'string' }, description: 'Concrete outcomes/requirements the user asks for on this topic.' },
    hows: { type: 'array', items: { type: 'string' }, description: 'Style/format/tools/references and HOW they want it done.' },
    rejections: {
      type: 'array',
      items: { type: 'object', required: ['item', 'why'], properties: { item: { type: 'string' }, why: { type: 'string' } } },
      description: 'Things they push back on / never want, each with the pattern behind it.',
    },
    workflow: { type: 'array', items: { type: 'string' }, description: 'The concrete steps they drive this kind of task through, in order.' },
    examples: { type: 'array', items: { type: 'string' }, description: 'Verbatim quotes / real references / specific constraints worth citing.' },
  },
}

phase('Read')
const found = await parallel(Array.from({ length: N }, (_, i) => () =>
  agent(
    `You are mining a user's own past messages (grouped by session) about: "${TOPIC}". Read the ENTIRE shard file /tmp/topic/shard_${pad(i)}.txt — every line, paginating with Read offset/limit to the end. Do NOT skim or skip. It is the user's own words (casual/profane in places — that's tone, extract the intent).\n\n` +
    `SCOPE — two rules:\n` +
    `(a) EXCLUDE the user's GENERAL working style that applies to every task regardless of topic: git/worktree/PR/rebase habits, plan→review→revise loops, autonomy/"lets go", commit conventions, billing/pricing. That general profile is captured elsewhere; don't re-derive it.\n` +
    `(b) KEEP everything OPERATIONAL for actually producing THIS deliverable — this is the most valuable part: the exact tools/providers/models they use, WHERE credentials live (e.g. keys in ~/.zshrc / env vars — capture the actual variable names/locations they mention), the render/generate/build commands, file paths, voice/model names, and any setup steps. These are topic-specific even though they look like "infra"; do NOT drop them.\n\n` +
    `Extract, grounded in their own words: what they WANT produced (outcomes, formats, quality bars); HOW they want it (style, tools, references); the SETUP/tooling (providers, key locations, commands — rule b); what they REJECT (with the pattern behind each); the STEPS specific to this work; and verbatim EXAMPLES/references. Capture EVERYTHING topic-specific in this shard.`,
    { label: `read:shard-${i}`, phase: 'Read', agentType: 'general-purpose', schema: FIND_SCHEMA }
  )
))
const all = found.filter(Boolean)

const merge = (key) => all.flatMap(f => f[key] || [])
const consolidated = {
  wants: merge('wants'),
  hows: merge('hows'),
  rejections: merge('rejections'),
  workflow: merge('workflow'),
  examples: merge('examples'),
}

phase('Consolidate')
const CONSOLIDATE_SCHEMA = { ...FIND_SCHEMA }
const canon = await agent(
  `Findings about "${TOPIC}" from ${N} agents that each read a different slice of the user's history:\n\n${JSON.stringify(consolidated, null, 1)}\n\n` +
  `Merge duplicates/overlaps into ONE comprehensive, deduped set. Keep it thorough — do not drop distinct real preferences to be brief; only collapse genuine repeats. Keep the strongest verbatim examples.`,
  { label: 'consolidate', phase: 'Consolidate', agentType: 'general-purpose', schema: CONSOLIDATE_SCHEMA }
)
const findings = canon || consolidated

phase('Write')
const skill_md = await agent(
  `Write a complete, ready-to-use Claude Code skill for the topic "${TOPIC}", named "${SKILL_NAME}", from these consolidated findings about how THIS user wants it done:\n\n${JSON.stringify(findings, null, 1)}\n\n` +
  `Output ONLY the file contents (no fences around the whole thing), with:\n` +
  `- YAML frontmatter whose \`name\` is EXACTLY \`${SKILL_NAME}\`; a description with real trigger phrases; allowed-tools if useful.\n` +
  `- Cover ONLY "${TOPIC}" — no general working-style rules (those live in a separate profile).\n` +
  `- ## When to use\n- ## What I want (grounded, cite their own words/examples)\n- ## Setup / tools & keys (the actual providers, models, credential locations like ~/.zshrc env vars, and render/generate commands — concrete, so this is runnable)\n- ## Watch out for / never do (each rejection with its why)\n- ## Workflow (their concrete ordered steps)\n` +
  `Be comprehensive and specific to this user — include the general standing preferences AND the specific tactical details. Do NOT hardcode secrets/tokens/IPs. Write in the user's own language where they do.`,
  { label: 'write-skill', phase: 'Write', agentType: 'general-purpose', effort: 'high' }
)

return { skill_md, findings }
