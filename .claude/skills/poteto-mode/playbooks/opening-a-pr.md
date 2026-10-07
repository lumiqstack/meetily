### Opening a PR

Invoked at the end of every code-changing playbook.

**Branch.** Work on a branch off `origin/main`, never on `main`. Name it `fix/<topic>` for bug fixes and `enhance/<topic>` for features, refactors, and tooling. PRs target `main` on `lumiqstack/meetily`. When the session runs in an app-made worktree, use the host's sync tool to bring it up to date with `main` instead of merging by hand.

**Commits.** Commit liberally. Rebase into small, ordered commits before opening the PR. Each commit is landable and ordered to tell the story: a failing repro before its fix, a subtraction before a reshape. Stage only the files you changed (`git add <files>`, never `-A`).

**Before review.** Run `/simplify` over the diff, then apply the Comments rule from `SKILL.md`. For a contested or risky change, run `/code-review high` and fix or dismiss each finding with a reason.

**Titles.** Conventional Commits, `type(scope): subject`. Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`. Scope is the changed area, such as `audio`, `transcription`, `summary`, `db`, `ui`, or `skills`. Short, imperative, no trailing period. Name a real symbol when one carries the change.

**Descriptions.** The body is a briefing, not the lab notebook. A reviewer who has the diff should learn why the change exists, what it leaves out, what it could break, and how you proved it works, in under a minute. Short, simple sentences with few identifiers. Use `##` headings in this order, and drop a section that has nothing to say:

- `## Why` gives the problem and the approach in one to three sentences.
- `## What changed` has one to three bullets. Name both sides of a rename or retarget.
- `## Scope` names what the PR covers and what it deliberately leaves out.
- `## Tradeoffs` names only rejected alternatives a reviewer would otherwise ask about.
- `## Blast Radius` gives one or two sentences on what the change touches (platforms, database schema, Tauri command or event shapes, audio threads) and why that is safe or risky.
- `## Verification` has one to three bullets, each a real run path and its outcome. Mark Windows-only paths that were not run on Windows. For a performance change, one primary number with its unit, `before → after`.

Attach screenshots when they prove a UI claim. Keep run logs and decision trails in a linked artifact, not the body.

**Readiness.** Open the PR ready, not as a draft, unless the user asks for a draft. Use `gh pr create --base main`. After opening, bind it with the session's PR tools when they are available, and report the URL. Opening a PR does not start a babysit. Don't merge or enable auto-merge unless the user asks.
