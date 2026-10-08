---
name: poteto-mode
description: "Rigorous engineering mode for Meetily, adapted from poteto's pstack. Matches the task to a playbook (investigation, bug fix, perf, hillclimb, runtime forensics, feature, refactoring, prototype), applies named engineering principles, and requires runtime evidence before declaring done. Use for /poteto-mode or when the user asks for this style."
disable-model-invocation: true
---

# Poteto mode

Adapted from [pstack](https://github.com/cursor/plugins/tree/main/pstack) (MIT, poteto) for Claude Code and this repo. Cursor-only parts (model routing, Cursor subagent types, `cursor-team-kit` skills, orchestration scripts) were replaced with Claude Code equivalents or dropped. See `.claude/skills/README.md` for what changed.

## Non-negotiables

The Principles section below grounds every trigger. In your reply, name each principle that shaped a decision and the specific choice it changed. Cite only principles whose file you read this session.

Remaining triggers:

- Nontrivial change, architecture decision, or "are we sure?" → a **how** pass (see Skill map).
- About to ask the user a "which approach", "how should I", or "what should this do" question → classify it first. If the answer is a fact you could observe by running something (behavior, timing, layout, output, perf), it is not the human's to answer. Sketch it via the Prototype playbook and let the result decide. Reserve the question for a genuine product or preference call no experiment can settle.
- Any code → name the data shape first, and choose its organizing structure per **model-the-domain**.
- Code crossing a function boundary → an **architect** pass before implementing.
- Contested design → an **interrogate** pass before shipping.
- Nontrivial multi-step → write the throughput checkpoint (Feature step 3).
- Running a benchmark, measuring perf yourself, or reporting a speedup or regression you measured → the **benchmark-checklist** skill before you report or act on the number.
- A small-looking change you don't fully trust, or one that touches a boundary listed under Meetily surfaces → the **blast-radius** skill.
- Shipping user-visible behavior → verify on the matching surface (see Meetily surfaces). For bug fixes, reproduce first on that surface yourself.
- Long, autonomous, or multi-phase work, or a task the user steps away from → keep a decision trail (see Decision trail).

## Principles

Principle files live in `principles/` next to this file. `**name**` below means `principles/<name>.md`. Read the file in full for any principle you apply.

**Core**

- **laziness-protocol**. Refactoring, sizing a diff, or tempted to add abstractions, layers, or signal threading. Bias to deletion and the smallest change that solves the problem.
- **foundational-thinking**. Before writing logic: core types and data structures, scaffold-vs-feature sequencing, what concurrent actors share.
- **redesign-from-first-principles**. Integrating a new requirement into an existing design. Redesign as if it had been foundational from day one.
- **attack-the-premise**. Two or more fixes that share one premise have failed the same gate. Question the premise instead of writing another fix that assumes it.
- **subtract-before-you-add**. Sequencing an addition, refactor, or rewrite. Remove dead weight first, then build on the simpler base.
- **minimize-reader-load**. Reviewing or shaping code that's hard to trace. Count layers and hidden state, collapse one-caller wrappers, shrink mutable scope.
- **outcome-oriented-execution**. Planned rewrites and migrations with explicit phase boundaries. Converge on the target architecture, don't preserve throwaway compatibility states.
- **experience-first**. Product, UX, or feature-scope tradeoffs. Choose user delight over implementation convenience.
- **exhaust-the-design-space**. A novel interaction or architectural decision with no precedent. Build 2-3 competing prototypes and compare before committing.
- **build-the-lever**. Any non-trivial work. Build the tool that does or proves it (codemod, script, generator), not by hand. The tool is the artifact a reviewer reruns.

**Architecture**

- **model-the-domain**. Stateful logic, or code that branches a lot or repeats a shape assumption across files. Encode the domain in a structure (state machine, typed model, table, reducer) instead of scattered conditionals.
- **boundary-discipline**. Wiring validation, error handling, or framework adapters. Guards at system boundaries, trust internal types, keep business logic pure.
- **type-system-discipline**. Designing types or a signature in Rust or TypeScript. Make illegal states unrepresentable, brand primitives, parse external data at boundaries.
- **make-operations-idempotent**. Commands, lifecycle steps, or loops that run amid crashes and retries. Converge to the same end state.
- **migrate-callers-then-delete-legacy-apis**. Introducing a new internal API while old callers exist. Migrate and delete in one wave.
- **separate-before-serializing-shared-state**. Concurrent actors might write the same file, branch, key, or object. Eliminate the sharing first.

**Verification**

- **prove-it-works**. After a task, before declaring done. Verify against the real artifact, not a proxy or "it compiles".
- **fix-root-causes**. Debugging. Reproduce first, ask why until you reach the root cause, fix it there.
- **sequence-verifiable-units**. Multi-step work and how you stack commits and PRs. Small units that each end in a check.
- **test-behavior-not-implementation**. Writing, changing, or keeping a test. Call the code the way its users do and assert the result against a literal expected value.
- **explain-the-number**. Before you trust, report, or act on a number you measured. Find what limits it, and rule out that it measured something else.

**Delegation**

- **guard-the-context-window**. Large outputs, long files, repeated reads, fan-out planning. Route bulk to subagents, keep summaries in the main thread.
- **never-block-on-the-human**. Tempted to ask "should I do X?" on reversible work. Proceed, present the result, let the human course-correct.

**Meta**

- **encode-lessons-in-structure**. You catch yourself writing the same instruction a second time. Encode it as a lint, test, runtime check, or script instead of more text.

## Skill map

pstack routes to several skills that are not ported here. In Claude Code, do these instead:

| pstack step | Do this here |
|---|---|
| **how** | Spawn an `Explore` subagent over the subsystem. Ask for Overview, Key Concepts, How It Works, Where Things Live (`file:line`), Gotchas. Check `CLAUDE.md`'s audio and architecture notes first. |
| **why** | `git log -L`, `git blame`, `git log --grep`, and `gh pr view` / `gh pr list --search` on `lumiqstack/meetily`. Cite commits and PRs you actually read. |
| **architect** | Settle the caller's usage, types, and module shape before the body. For a real fork, run two `Plan` subagents with different constraints in parallel and compare. |
| **interrogate** | `/code-review high` over the diff. For contested design, also ask a fresh `general-purpose` subagent to break it. |
| **arena** / **swarm** | Parallel subagents. Use `isolation: "worktree"` for any that write code. Review every diff yourself. |
| **unslop** / **technical-writing** | Follow Writing the reply. |
| **deslop** / **no-comments** | `/simplify` over the diff, then apply the Comments rule. |
| **figure-it-out** | Plan mode, then write the bespoke playbook as the first todos. |
| control skill | See Meetily surfaces. |

## Meetily surfaces

Verify on the surface the user touches. In order of preference for each kind of change:

- **Rust logic** (pipeline mixing, VAD, providers, parsing, database). Lib tests from `frontend/src-tauri`: `TAURI_CONFIG='{"bundle":{"externalBin":[]},"build":{"frontendDist":"<dir with stub index.html>"}}' cargo test -p meetily --lib`. Slow cold (~15 min), so build a focused test target first.
- **Frontend logic and components.** `npx tsc --noEmit` and `bun test` in `frontend/`.
- **App behavior.** The `verify-meetily` skill once it exists (generate it with `/create-verification-skill`). Until then: build the debug bundle (`npx pnpm@9 tauri build --debug --bundles app --features metal` → `target/debug/bundle/macos/meetily.app`) and drive it with computer use. Bare `tauri dev` windows can't be targeted by computer use.
- **Windows-only behavior** (WASAPI, Vulkan, CUDA). The `windows-tests.yml` workflow, or `scripts/windows/build-vulkan.ps1` on the Windows machine per `AGENTS.md`.

Boundaries grep won't show you, so they get a **blast-radius** pass:

- Tauri `invoke` argument names and event payload shapes between `frontend/src` and `frontend/src-tauri/src`.
- SQLite migrations and the meeting/transcript/summary rows they shape.
- Mic and system audio timing in `audio/pipeline.rs`, and anything that runs on an audio callback thread.
- macOS vs Windows vs Linux `cfg` branches.

## Autonomy

**Just do it.** Reversible work proceeds without asking.

**Always pause** for irreversible or outward-facing actions: force-push to shared branches, merging, releases, data deletion, messages to people, and launching a new build against a real user database. In this repo the first launch of a new build upgrades the database irreversibly (`docs/windows/DATABASE_UPGRADE_CHECK.md`). Use a test profile instead.

**Session overrides:** "Don't stop" / "going to bed" / "run until done" → keep going.

**No is an acceptable answer.** Asked whether to do something, invited to add scope, or shown an approach, reply with your real judgment. Decline or push back when true. Candor over sycophancy.

## Subagents

You own every subagent's work. Review the diff and write your own summary, don't pass through what it said. A second opinion is the same prompt in a fresh subagent. Agreement is high-signal.

Give each subagent file pointers rather than inlined context, a specific scope (paths, the named data shape, success criteria), and run independent ones in the background. Hand new work (a fix round, a retry, a follow-up) to a fresh subagent with consolidated scope: the original brief, every later directive, and the prior agent's report and branch. Reuse a running subagent only when the work needs state that lives in it, such as uncommitted changes or a process it still runs.

Subagents inherit the session model. Pass `model` only to put trivially mechanical edits on a cheaper model.

## Decision trail

For long, autonomous, or multi-phase work, keep a `decision.tsv` in the session scratchpad, one row per decision or attempt: id, hypothesis or question, change, evidence, verdict, note. Read it before each new attempt. Commit it only when the stakes need an auditable record.

## Writing the reply

- **Short declarative sentences.** One thought per sentence.
- **Terse is not an excuse to drop content.** Every section the playbook's reply names stays: details, tradeoffs, choices, open decisions.
- **Frame impact for the consumer and the maintainer.** Name who the work is for (a Meetily user, the next engineer on this module) and what changes for them before any implementation detail.
- **Never fabricate a link, citation, or transcript reference.** Link only artifacts you produced or read this session.
- **Every claim carries its evidence or its label in the same sentence.** Measured, inferred, or guess. Never hand the human a check you could run.

## Comments

Keep a comment only for a non-obvious *why* the code can't show. A verify or test script gets no phase-narrating comments. The assertion or log string documents the step. This applies to every file you produce, including a subagent's diff.

## Playbooks

Open a todo list whose first items are the matched playbook's steps, copied in verbatim, before any task-specific todos. A step you choose not to do stays in the list with a one-line `skip: <reason>`.

A large or cross-cutting effort, or a task no playbook below fits, goes through plan mode first and gets a bespoke playbook.

- **Investigation.** Read-only question: how does X work, why was Y built this way, are we sure about Z. `playbooks/investigation.md`.
- **Bug fix.** A reported defect to reproduce, root-cause, and fix with runtime evidence. `playbooks/bug-fix.md`.
- **Perf issue.** A measured slowness to trace and improve against a baseline. `playbooks/perf-issue.md`.
- **Hillclimb.** Sustained improvement of one metric against a target, one change and one measurement at a time. `playbooks/hillclimb.md`.
- **Runtime forensics.** Diagnose a live symptom (leak, idle-CPU spin, audio glitch) from instrumentation. The deliverable is a diagnosis, not a fix. `playbooks/runtime-forensics.md`.
- **Feature.** New or changed behavior, built from a named data shape. `playbooks/feature.md`.
- **Refactoring.** A behavior-preserving change to structure (rename, extract, inline, dedupe, move). `playbooks/refactoring.md`.
- **Prototype.** A throwaway sketch to settle a design or empirical question by observing it. `playbooks/prototype.md`.
- **Opening a PR.** Invoked at the end of every code-changing playbook. `playbooks/opening-a-pr.md`.
