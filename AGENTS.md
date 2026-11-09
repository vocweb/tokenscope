## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5.Results Report

- State the executed command along with the actual output; do not use "all tests passed" as a completion report.
- Specify which assertions were proven capable of failing and how that was demonstrated.
- Identify which scenarios the test harness fails to cover.
- Claims that a failure is pre-existing must be supported by evidence from running the unmodified baseline.
- Do not claim a task is "done" without accompanying verification output.

## 6. Repo Conventions (MUST, every agent)

These repeat the user-scope rules on purpose: they are the ones agents skip most. They apply to Claude Code, Codex, omp and any other agent.

- Never use the em dash character (U+2014) in code, comments, commits, docs, or MR/PR text. Use a plain "-".
- Reply in the language the user uses. Vietnamese replies always use full diacritics. Plans/specs are written in the language of the conversation.
- Source-code comments answer "why it was done this way", in exactly two lines: one Japanese, one Vietnamese (full diacritics).
- Never add the agent as co-author in commit messages. Never edit CHANGELOG.md or auto-generated files.
- Do not push, merge, or open a PR/MR unless the user asks. If the history contains a credential, stop and tell the user before pushing.
- Fix lint errors, failing tests and flaky tests you see, even when not caused by your change.
- Quality comes first. Never choose a patchwork or band-aid solution. Before proposing a solution, check the whole codebase to make sure it does not affect other components.
- Before presenting a proposal or solution for the human to choose, spawn a sub-agent to critique it. The critique must answer three questions:
  1. WHY: why does this solution need to be done here?
  2. Is there a better solution? Has the quality of this solution reached the highest level?
  3. Is the solution correct and right for the long term? If not, find another solution and repeat the critique.

  If the agent cannot spawn a sub-agent, it critiques the solution itself against the same three questions and tells the user that the critique was done without a sub-agent.

## 7. Text search: use `rg` (MUST)

- Use `rg` for every shell search (content and file names). Never use `grep`, `egrep`, `fgrep`, `git grep`, or `find | xargs grep`.
- Fall back to `grep` only where `rg` is unavailable (POSIX scripts for CI images, containers, git hooks). State the reason.
- Do not add `-r` or `-n`: `rg` is recursive, numbers lines, and skips ignored, hidden and binary files by default.
- Always narrow the search: a path, `-g`, `-t`, `-m`, or `-l` first.
- Quote patterns in single quotes. Use `-F` for literals, `-P` only for look-around or back-references.
- Treat exit code `1` as "no match", not an error (`rg ... || true` in scripts).
- The Claude Code `Grep` tool runs ripgrep and is allowed.

Flags shared with `grep`: `-F -i -w -l -c -v -o -e -m -A -B -C`.

| Need | Use |
|---|---|
| Smart case | `rg -S` |
| Files without a match (`grep -L`) | `rg --files-without-match` |
| Include / exclude glob | `rg -g '*.ts' -g '!node_modules/'` |
| By file type | `rg -t ts`, `rg -T js` (exclude), `rg --type-list` |
| Hidden / ignored files | `rg --hidden`, `rg --no-ignore`, `-u` / `-uu` / `-uuu` |
| Multi-line | `rg -U 'start.*\n.*end'` |
| Find files by name (`find -name`) | `rg --files -g '*.vue'` |
| Machine-readable output | `rg --json` |

## 8. Pull / Merge requests (MUST)

Creating or updating a PR/MR by any tool (`gh`, `glab`, `glab-axi`, API) means following the repo template. A skill's own generic workflow never overrides this section.

1. Read the template first: `.github/PULL_REQUEST_TEMPLATE/pull_request_template.md` by default, `bugfix_template.md` for bug fixes, `doc_template.md` for docs-only changes. `.gitlab/merge_request_templates/Default.md` is the same as the default one.
2. Fill every section from the real diff and the real verification results. Body text is Japanese first, then the Vietnamese translation on the next line (full diacritics). Headings and table headers are one line "Japanese - Vietnamese". Keep code, commands, paths, URLs and identifiers untranslated. When a section has sub-items, structure them with Markdown (sub-headings, lists, tables) for human readability instead of long paragraphs. Mermaid diagrams are welcome; node labels also keep the Japanese/Vietnamese pair as `A["Japanese<br>Vietnamese"]`, and edge labels are not used (see the template notes for the GitHub/GitLab rendering limits).
3. Never claim something is verified unless you ran it. Mark non-applicable items "該当なし" with the reason and the Vietnamese line.
4. Remove all `<!-- -->` guidance comments, write the finished body to a file, and pass it with `--body-file`. The title follows the same "Japanese - Vietnamese" form when the template says so.
5. In Claude Code, `.claude/hooks/pr-template-guard.mjs` (wired in `.claude/settings.json`) blocks create/update commands whose body is not template-shaped. Other agents have no hook, so you must check this yourself before running the command.
6. GitLab host is `git.smb-base.net`; run `glab-axi` through `zsh -ic` with `GITLAB_HOST=git.smb-base.net`. Never claim CI is green without a pipeline.

## 9. Maximize Laravel 13 and Nuxt 4 Capabilities (MUST, every agent)

- Backend: before creating a plan and before implementation, MUST read `docs/reference/laravel/13.x/agent/INDEX.md` and the files referenced in it to identify built-in Laravel 13 capabilities that meet the requirements.
- Frontend: before creating a plan and before implementation, MUST read `docs/reference/nuxt/4.x/agent/INDEX.md` and the files referenced in it to identify built-in Nuxt 4 capabilities that meet the requirements.
- Tasks involving both backend and frontend MUST satisfy both reading requirements before planning and before implementing the respective parts.
- MUST make full use of applicable built-in APIs, helpers, conventions, and mechanisms in Laravel 13 and Nuxt 4. Ground decisions in the reference documentation rather than relying only on memory or knowledge of older versions.
- MUST check framework capabilities before proposing or writing custom functionality. Build custom functionality only when built-in capabilities do not meet the requirements, and explain the gap and rationale in the plan or implementation explanation. NEVER recreate functionality that the framework already provides and that meets the requirements.

## Agent skills

### Issue tracker

Before reading or publishing issues, follow the local Markdown workflow in `docs/agents/issue-tracker.md`.

### Triage labels

Before recording triage status, use the five canonical role mappings in `docs/agents/triage-labels.md`.

### Domain docs

Before exploring domain code, follow the single-context glossary and ADR rules in `docs/agents/domain.md`.
