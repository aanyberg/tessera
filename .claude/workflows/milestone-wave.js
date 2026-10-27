export const meta = {
  name: 'milestone-wave',
  description: 'Run one wave of milestone issues through the Tessera software factory: implement → critic loop (rust/swift/code critic by area, ≤5) → quality-gate ∥ adversarial-tester → serialized integrator merge',
  whenToUse: 'When the milestone-lead has produced task briefs for a wave and the repo is clean on main.',
  phases: [
    { title: 'Research', detail: 'spec-researcher for briefs flagged spec:true' },
    { title: 'Implement', detail: 'one implementer per brief in its own worktree' },
    { title: 'Critique', detail: 'rust-/swift-/code-critic verdicts by area; revision-implementer between rounds' },
    { title: 'Verify', detail: 'quality-gate and adversarial-tester in parallel' },
    { title: 'Merge', detail: 'integrator, one branch at a time' },
  ],
}

const { tasks, repo, wtRoot } = args
const MAX_CRITIQUE_ROUNDS = 5
const MAX_VERIFY_CYCLES = 2

// Commit trailer lines; pass `attribution` in args to match the running session's attribution reminder.
const ATTRIB = args.attribution || 'Co-Authored-By: Claude <noreply@anthropic.com>'

const IMPL_SCHEMA = { type: 'object', properties: {
  summary: { type: 'string' }, files_changed: { type: 'array', items: { type: 'string' } },
  how_verified: { type: 'string' }, checks_pass: { type: 'boolean' },
  open_concerns: { type: 'array', items: { type: 'string' } },
}, required: ['summary', 'files_changed', 'how_verified', 'checks_pass', 'open_concerns'] }
const CRITIQUE_SCHEMA = { type: 'object', properties: {
  verdict: { type: 'string', enum: ['ship', 'revise'] }, rationale: { type: 'string' },
  findings: { type: 'array', items: { type: 'object', properties: {
    severity: { type: 'string', enum: ['blocker', 'major', 'minor'] }, file: { type: 'string' }, line: { type: 'integer' },
    description: { type: 'string' }, fix: { type: 'string' } }, required: ['severity', 'file', 'description', 'fix'] } },
}, required: ['verdict', 'rationale', 'findings'] }
const VERIFY_SCHEMA = { type: 'object', properties: {
  pass: { type: 'boolean' }, evidence: { type: 'string' },
  issues: { type: 'array', items: { type: 'object', properties: {
    severity: { type: 'string', enum: ['blocker', 'major', 'minor'] }, description: { type: 'string' }, repro: { type: 'string' } },
    required: ['severity', 'description', 'repro'] } },
}, required: ['pass', 'evidence', 'issues'] }
const MERGE_SCHEMA = { type: 'object', properties: {
  merged: { type: 'boolean' }, commit: { type: 'string' },
  conflicts_resolved: { type: 'array', items: { type: 'string' } }, notes: { type: 'string' },
}, required: ['merged', 'commit', 'conflicts_resolved', 'notes'] }
const RESEARCH_SCHEMA = { type: 'object', properties: {
  fixture_path: { type: 'string' }, sources: { type: 'array', items: { type: 'string' } },
  decisions_needed: { type: 'array', items: { type: 'string' } }, summary: { type: 'string' },
}, required: ['fixture_path', 'sources', 'decisions_needed', 'summary'] }

const wtOf = t => `${wtRoot}/task-${t.n}`
const branchOf = t => `task/${t.n}-${t.slug}`
const implementerFor = t => t.area === 'app' ? 'swift-implementer' : t.area === 'infra' ? 'release-engineer' : t.area === 'docs' ? 'docs-writer' : 'rust-implementer'
const criticFor = t => t.area === 'app' ? 'swift-critic' : (t.area === 'infra' || t.area === 'docs') ? 'code-critic' : 'rust-critic'

const brief = t => `Issue #${t.n} (milestone ${t.milestone || '0.1.0'}) — ${t.title}
Area: ${t.area}. Scope: ${t.scope}.
Repo: ${repo}. Worktree: ${wtOf(t)}. Branch: ${branchOf(t)}.
${t.assignments ? 'Pre-assigned by milestone-lead: ' + JSON.stringify(t.assignments) + '\n' : ''}
${t.body}

Done when: ${t.done}

Commit trailer required on every commit (two lines, verbatim):
${ATTRIB}`

const findingsBlock = (round, findings) => `Review round ${round} returned "revise". Findings to address:
${JSON.stringify(findings, null, 2)}`

let chain = Promise.resolve()
const serialized = fn => { const p = chain.then(fn, fn); chain = p.catch(() => {}); return p }

const results = await pipeline(tasks,
  async (t) => {
    const s = { t, rounds: [], verification: [] }
    if (t.spec) {
      s.research = await agent(`Produce the spec fixture for this issue before implementation starts. Write it into the repo at ${repo} under the owning crate's tests/fixtures/ (do not commit; the implementer will pick it up from the path you return).\n\n${brief(t)}`,
        { agentType: 'spec-researcher', label: `research:#${t.n}`, phase: 'Research', schema: RESEARCH_SCHEMA })
    }
    return s
  },
  async (s) => {
    const { t } = s
    const fixture = s.research ? `\nSpec fixture prepared by spec-researcher: ${s.research.fixture_path}\n${s.research.summary}\nOpen decisions: ${s.research.decisions_needed.join('; ') || 'none'}\n` : ''
    s.impl = await agent(`Implement this issue in its own worktree.\n${fixture}\n${brief(t)}`,
      { agentType: implementerFor(t), label: `impl:#${t.n}`, phase: 'Implement', schema: IMPL_SCHEMA })
    return s
  },
  async (s) => {
    const { t } = s
    for (let round = 1; round <= MAX_CRITIQUE_ROUNDS; round++) {
      const c = await agent(`Critique round ${round} of ${MAX_CRITIQUE_ROUNDS}.\n\n${brief(t)}`,
        { agentType: criticFor(t), label: `critique:#${t.n}:r${round}`, phase: 'Critique', schema: CRITIQUE_SCHEMA })
      if (!c) { s.rounds.push({ round, verdict: 'error' }); continue }
      s.rounds.push({ round, verdict: c.verdict, findings: c.findings })
      log(`#${t.n} critique r${round}: ${c.verdict} (${c.findings.length} findings)`)
      if (c.verdict === 'ship') { s.shipped = true; break }
      if (round === MAX_CRITIQUE_ROUNDS) break
      s.rounds[s.rounds.length - 1].fix = await agent(`${findingsBlock(round, c.findings)}\n\nImplementer's summary: ${s.impl?.summary || 'n/a'}\n\n${brief(t)}`,
        { agentType: 'revision-implementer', label: `revise:#${t.n}:r${round}`, phase: 'Critique', schema: IMPL_SCHEMA })
    }
    s.shipped = !!s.shipped
    return s
  },
  async (s) => {
    const { t } = s
    if (!s.shipped) { log(`#${t.n}: not shipped after ${MAX_CRITIQUE_ROUNDS} rounds; skipping verification`); return s }
    const lenses = [
      { lens: 'quality', agentType: 'quality-gate', prompt: `Run the mechanical gate on this branch.\n\n${brief(t)}` },
      { lens: 'adversarial', agentType: 'adversarial-tester', prompt: `The critic marked this branch ship-ready. Break it.\n\n${brief(t)}` },
    ]
    for (let cycle = 1; cycle <= MAX_VERIFY_CYCLES; cycle++) {
      const vs = await parallel(lenses.map(l => () =>
        agent(l.prompt, { agentType: l.agentType, label: `verify:${l.lens}:#${t.n}:c${cycle}`, phase: 'Verify', schema: VERIFY_SCHEMA })
          .then(v => ({ lens: l.lens, ...(v || { pass: false, evidence: 'verifier died', issues: [] }) }))))
      // any major/blocker fails the lens regardless of its own pass flag
      const failed = vs.filter(Boolean).filter(v => !v.pass || v.issues.some(i => i.severity !== 'minor'))
      s.verification.push({ cycle, results: vs })
      log(`#${t.n} verify c${cycle}: ${failed.length ? 'FAIL (' + failed.map(f => f.lens).join(',') + ')' : 'pass'}`)
      if (!failed.length) { s.verified = true; break }
      if (cycle === MAX_VERIFY_CYCLES) { s.verified = false; break }
      const issues = failed.flatMap(f => f.issues.map(i => ({ ...i, lens: f.lens, file: '', fix: i.repro })))
      await agent(`${findingsBlock('verification-' + cycle, issues)}\n\n${brief(t)}`,
        { agentType: 'revision-implementer', label: `revise:#${t.n}:v${cycle}`, phase: 'Verify', schema: IMPL_SCHEMA })
    }
    return s
  },
  async (s) => {
    const { t } = s
    if (!s.verified) { s.merge = { merged: false, notes: s.shipped ? 'verification failed' : 'critique never reached ship' }; return s }
    s.merge = await serialized(() => agent(`Squash-merge ${branchOf(t)} into local main.\n\n${brief(t)}`,
      { agentType: 'integrator', label: `merge:#${t.n}`, phase: 'Merge', schema: MERGE_SCHEMA }))
    log(`#${t.n} merge: ${s.merge?.merged ? 'merged ' + s.merge.commit : 'NOT merged'}`)
    return s
  },
)

return results.filter(Boolean).map(s => ({
  issue: s.t.n, title: s.t.title, branch: branchOf(s.t), worktree: wtOf(s.t),
  research: s.research ? { fixture: s.research.fixture_path, decisions_needed: s.research.decisions_needed } : null,
  critique_rounds: s.rounds.map(r => ({ round: r.round, verdict: r.verdict, findings: (r.findings || []).map(f => `${f.severity}: ${f.file}${f.line ? ':' + f.line : ''} ${f.description}`) })),
  shipped: s.shipped, verified: s.verified,
  verification: s.verification.map(c => ({ cycle: c.cycle, lenses: c.results.filter(Boolean).map(v => ({ lens: v.lens, pass: v.pass, issues: v.issues.map(i => `${i.severity}: ${i.description}`) })) })),
  merge: s.merge, impl_concerns: s.impl?.open_concerns || [],
}))
