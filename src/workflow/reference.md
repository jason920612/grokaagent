# Workflow authoring guide

A workflow is a JavaScript script that orchestrates many subagents. Use it to be comprehensive (split the work and cover it in parallel), to be confident (independent checks and adversarial verification before you commit to an answer), or to handle more than one context can hold (sweeps across a whole codebase, audits, migrations).

Often the right move is hybrid: scout yourself first (list the files, read the diff, find the modules) to get the work list, then write a workflow that fans out over it. You only need the shape of the work before the orchestration step, not before the task.

Common single-phase workflows you can chain across turns:
- Understand: parallel readers over subsystems → a structured map
- Design: several independent approaches → scored by parallel judges → synthesis
- Review: one finder per dimension → each finding adversarially verified
- Research: several search angles → deep reads → synthesis
- Migrate: find the sites → transform each (worktree isolation) → verify

For larger work, run several in sequence and read each result before you decide the next one.

## Running one

Call `workflow` with `script` (the whole text), `script_path` (a .js file in the workspace) or `name` (a saved workflow). It returns at once with a `run_id`; the run goes on in the background and a `[workflow wf-… finished]` notice with the script's return value arrives when it ends. Do not poll: keep working or end your turn. `stop_workflow` stops a run.

Each run saves its script to `.groka/workflow-runs/<run_id>/script.js`, with `journal.jsonl` (every finished agent call), `result.json` and the agents' event logs under `agents/`. To iterate, edit the script file and call `workflow` with `script_path` and `resume_from_run_id`: every `agent()` call whose prompt and options did not change returns its recorded result instantly; changed and new calls run live. Calls are matched by content (prompt, schema, model, effort, isolation) and by how many times that same call was made, not by position, so reordering is fine. Labels and phases are display only and do not affect matching.

Saved workflows live in `.groka/workflows/<name>.js` (the project) and `~/.grokaagent/workflows/<name>.js` (the user). Run one with `workflow {"name": "<name>", "args": …}`.

## Script shape

Every script begins with `export const meta = {...}`, a PURE LITERAL (no variables or calls):

```js
export const meta = {
  name: 'find-flaky-tests',
  description: 'Find flaky tests and propose fixes',
  phases: [
    { title: 'Scan', detail: 'grep test logs for retries' },
    { title: 'Fix', detail: 'one agent per flaky test' },
  ],
}
phase('Scan')
const flaky = await agent('Find tests that were retried in the CI logs under logs/. List each test id and the evidence.', { schema: FLAKY })
// ...
return { flaky }
```

The rest of the file runs as the body of an async function: `await` and `return` work at the top level, and the return value (JSON) is the run's result. Plain JavaScript only: no TypeScript annotations, no `import`/`require`. There is no file, network or timer access in the script itself; agents do the work.

## API

- `agent(prompt, opts?)` → Promise. Starts one subagent and resolves with its final text, or with the validated object when `opts.schema` is given, or with `null` if the agent failed (an error, its turn limit, a result that never fit the schema). It throws when the run is stopped, when the token budget is spent, or past the agent limit.
  - `label`: a short name shown in the agent tree.
  - `phase`: groups the agent under that phase (otherwise the last `phase()` call).
  - `schema`: a JSON Schema whose root is `{type: 'object', ...}`. The agent hands in its result through a `submit_result` tool with that schema; a result that does not fit is sent back to it to fix. The validator understands type, properties, required, additionalProperties, items, enum, const, anyOf/oneOf/allOf, min/maxItems, min/maxLength, minimum/maximum.
  - `model`: a model id for this agent (default: the session's child model, else the main model). Omit it unless you are sure another model fits.
  - `effort`: 'low' | 'medium' | 'high' | 'xhigh' reasoning effort; use 'low' for cheap mechanical stages.
  - `isolation: 'worktree'`: run the agent in a fresh git worktree (`git worktree add --detach HEAD` under `.groka/worktrees/`). Expensive; use it only when agents edit files in parallel and would conflict. An unchanged worktree is removed; one with changes is kept on a branch `groka-workflow/<name>` (its work committed) and named in the run's log.
- `parallel(thunks)` → Promise of an array. Runs every thunk at once and waits for ALL of them (a barrier). A thunk that throws becomes `null` in the result; filter with `.filter(Boolean)`.
- `pipeline(items, stage1, stage2, ...)` → Promise of an array. Each item goes through all stages on its own, with NO barrier between stages: item A can be in stage 3 while item B is still in stage 1. Every stage gets `(prevResult, originalItem, index)`. A stage that throws drops that item to `null` and skips its remaining stages.
- `phase(title)`: starts a phase; later agents without their own `phase` go under it.
- `log(message)`: a progress line in the workflow's log.
- `args`: the `args` value the run was started with (verbatim JSON).
- `budget`: `{ total, spent(), remaining() }`. `total` is the token target of the user's latest message (a `+500k` or `+2m` in it), or `null` without one. Spending counts output tokens plus input tokens the prompt cache did not cover, across the main agent and every agent this turn. It is a hard ceiling: once spent reaches total, `agent()` throws. With no target, `remaining()` is `Infinity`, so guard loops on `budget.total`.
- `workflow(nameOrRef, args?)`: runs a saved workflow (by name) or `{scriptPath}` inline as a sub-step and returns its result. It shares this run's agent limit, stop and budget. Nesting is one level: a nested workflow cannot call `workflow()`.

`Date.now()`, `new Date()` without arguments and `Math.random()` throw: they would make a resumed run behave differently. Pass timestamps in through `args`; vary prompts by index instead of randomness.

Limits: at most {MAX_CONCURRENCY} agents of a run work at once (the rest wait their turn), 1000 agents per run, 4096 items per `parallel()`/`pipeline()` call. Await every `agent()` call: calls still running when the script returns are stopped.

## Writing good agent prompts

Each agent starts with nothing but its prompt: no conversation, no view of the other agents. Write each prompt as a complete brief: the goal, the relevant paths, the constraints, and what to return (with `schema` for anything the script reads). Agents share the workspace and have the full tool set (files, shell, search), so tell parallel agents which files are theirs, or use `isolation: 'worktree'`.

## pipeline() by default

Use `pipeline()` for multi-stage work. Reach for a barrier only when a stage needs the results of ALL items of the stage before it:
- deduplicate or merge across everything found before expensive verification
- stop early when nothing was found
- a prompt that compares against "the other findings"

A barrier is NOT needed to flatten, map or filter: do that inside a pipeline stage. If you wrote `const a = await parallel(...); const b = transform(a); await parallel(b.map(...))` and `transform` has no cross-item dependency, make it one pipeline.

## Patterns

Review with adversarial verification:

```js
export const meta = { name: 'review-changes', description: 'Review the diff by dimension and verify each finding', phases: [{ title: 'Review' }, { title: 'Verify' }] }
const FINDINGS = { type: 'object', properties: { findings: { type: 'array', items: { type: 'object', properties: { file: { type: 'string' }, line: { type: 'integer' }, claim: { type: 'string' } }, required: ['file', 'claim'] } } }, required: ['findings'] }
const VERDICT = { type: 'object', properties: { real: { type: 'boolean' }, why: { type: 'string' } }, required: ['real', 'why'] }
const DIMENSIONS = ['correctness', 'error handling', 'security']
const results = await pipeline(
  DIMENSIONS,
  d => agent(`Review the uncommitted diff (git diff) for ${d} problems only. Cite file and line.`, { label: `review-${d}`, phase: 'Review', schema: FINDINGS }),
  r => parallel((r?.findings ?? []).map(f => () =>
    agent(`Try to REFUTE this claim about the code: ${JSON.stringify(f)}. Read the code. If unsure, say it is not real.`, { phase: 'Verify', schema: VERDICT, effort: 'high' })
      .then(v => ({ ...f, verdict: v }))))
)
return results.flat().filter(f => f && f.verdict?.real)
```

Majority vote: run 3 independent refuters and keep a claim only if at least 2 fail to refute it.

```js
const votes = await parallel([0, 1, 2].map(i => () => agent(`Skeptic #${i + 1}: try to refute ${claim}. Default to refuted=true if uncertain.`, { schema: VERDICT })))
const survives = votes.filter(Boolean).filter(v => !v.refuted).length >= 2
```

Perspective-diverse checks: when a finding can be wrong in more than one way, give each checker a different lens (correctness, security, does it reproduce) instead of N identical ones.

Judge panel: several independent attempts from different angles, scored by parallel judges, then a synthesis built from the best one plus the best ideas of the rest.

Loop until dry, for discovery of unknown size: keep running finders until two rounds in a row find nothing new. Deduplicate against everything SEEN (not only what was confirmed), or rejected findings come back every round:

```js
const seen = new Set(), confirmed = []
let dry = 0
while (dry < 2) {
  const found = (await parallel(FINDERS.map(f => () => agent(f, { phase: 'Find', schema: BUGS })))).filter(Boolean).flatMap(r => r.bugs)
  const fresh = found.filter(b => !seen.has(b.file + ':' + b.line))
  if (!fresh.length) { dry++; continue }
  dry = 0
  fresh.forEach(b => seen.add(b.file + ':' + b.line))
  const judged = await parallel(fresh.map(b => () => agent(`Is this bug real? ${JSON.stringify(b)}`, { phase: 'Verify', schema: VERDICT })))
  confirmed.push(...fresh.filter((b, i) => judged[i]?.real))
}
return confirmed
```

Loop until budget:

```js
while (budget.total && budget.remaining() > 50_000) { /* another round */ }
```

Completeness critic: a last agent that asks what is missing (an angle not searched, a claim not verified, a file not read); what it finds is the next round of work.

No silent caps: if the script limits coverage (top N, sampling, no retry), `log()` what was left out, so the result does not read as "covered everything".

Scale to what was asked: "find any bugs" → a few finders and one verification each; "audit thoroughly" → more finders, 3–5 votes per finding, a synthesis stage. Keep to the session's workflow size guideline when one is given.
