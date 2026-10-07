# Project skills

Claude Code loads each folder here that has a `SKILL.md` as a project skill.

These skills are adapted from [pstack](https://github.com/cursor/plugins/tree/main/pstack) by poteto, a Cursor plugin, released under the MIT license (copy in `PSTACK-LICENSE`).

| Skill | Use it for |
|---|---|
| `/poteto-mode` | Any task that needs rigor. Matches the task to a playbook and applies the principles in `poteto-mode/principles/`. |
| `/blast-radius` | What a change could break beyond the diff, with the key safety fact proven by running code. |
| `/benchmark-checklist` | Vetting a measured speedup or regression before reporting or acting on it. |
| `/tdd` | A bug with a cheap local test path. Failing test first, then the fix. |
| `/create-verification-skill` | Generating a `verify-meetily` skill that drives the real app and captures evidence. |

## Changes from upstream

- Only 8 of the 23 `poteto-mode` playbooks are kept: investigation, bug fix, perf issue, hillclimb, runtime forensics, feature, refactoring, prototype. Opening a PR is rewritten for this repo.
- The 24 principle skills moved into `poteto-mode/principles/` as plain files, so they don't crowd the `/` menu.
- Cursor-specific parts are replaced with Claude Code equivalents (see the Skill map in `poteto-mode/SKILL.md`): model routing, Cursor subagent types, `cursor-team-kit` skills, and pstack skills not ported here (`how`, `why`, `architect`, `arena`, `swarm`, `interrogate`, `unslop`, `no-comments`, `show-me-your-work`, `figure-it-out`).
- The orchestration and PR-watch scripts are dropped.
- Meetily specifics are added: verification surfaces, boundaries that need a blast-radius pass, and the irreversible database upgrade on first launch.
