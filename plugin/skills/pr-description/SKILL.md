---
name: pr-description
description: Use when a pull request is about to be opened or its description updated — "write the PR", "describe this branch", "draft the PR body/title", before running `gh pr create` or `gh pr edit`, or when new commits land on a branch that already has a PR. Also use when a description must state what testing was actually done and what was not.
argument-hint: "[base-branch]"
---

# PR description

A PR description is written for a reviewer who has not read the commits and will
not read the diff first. It supplies what the diff cannot: why the change exists,
what was verified, where to look, and what could break. The description matches
the shape below or it is not finished.

Invoke as `/dispatch:pr-description [base]`, or let it load from context; `base`
defaults to the repo's default branch.

## Gather before writing, in this order

1. **Commits:** `git log --format='%h %s%n%b' origin/<base>..HEAD`. The *why* comes
   from a commit body, a linked issue or card, a spec or plan committed in the
   repo, or the author. **If none of those states a motivation, ask the author
   before writing; when nobody can be asked (an unattended run), write
   "Motivation: not stated in the commits or the card" and flag it under Review
   focus. A motivation you inferred is a guess presented as fact.**
2. **Diff:** `git diff origin/<base>...HEAD`. Read it; `--stat` alone cannot support
   a claim such as "comment-only changes". Every statement about what the change
   does or does not touch is checked against the diff, including incidental
   changes (an autofix, a drive-by removal, a contradiction resolved).
3. **The repo's definition of done.** Take it from the repo's `CLAUDE.md` or
   contributing guide when it states one; otherwise derive it from what is
   configured: a `make lint`/`test`/`typecheck` target, `.pre-commit-config.yaml`,
   linter sections in `pyproject.toml`, test markers, `package.json` scripts, CI
   workflow steps. Run those checks on the touched files and keep the output.
   Nothing that was not run in this session appears as run.
4. **Existing PR:** `gh pr view --json number,url,body`. If one exists, you are
   updating its body, not writing a second one.

## The shape

Every description has these parts in this order. A part marked *when* is included
only when its condition holds; the rest are always present. A reviewer reads the
whole thing in two minutes: about 400 words for a normal branch, 700 for one that
touches more than twenty files.

**Title** — `type(scope): summary`, Conventional Commits, imperative, ≤72
characters, the same contract the repo applies to commits. Add `!` after the scope
*when* an interface, schema, env var, hook stage, or command contract changes, and
repeat it as a `BREAKING CHANGE:` line in the body.

**Summary** — two to five sentences: the problem, the approach, and why this
approach rather than the obvious alternative. A number appears only when it
changes what the reviewer does (`129 → 103 lines`, `75 of 120 commits`).

**What changed** — at most eight bullets, grouped by intent, never by file. The
diff is the inventory; a bullet says what a group of changes achieves, and names
a file only where the reviewer has to open it. Incidental changes get their own
bullet so nobody discovers them in the diff: "the lint hook dropped one
pre-existing unused import".

**Verification** — a table, always present:

| Command | Scope | Result |
|---|---|---|
| `ruff check src/billing tests/billing` | touched files | clean |
| `pytest tests/billing -m "not slow"` | touched area | 41 passed |
| `pytest tests/ -m "not slow"` | full hermetic | **Not run:** no local database on this machine |

Followed by one line per thing *not* verified and why (a live provider, a
migration against real data, a UI you could not open). A row is either a command
you ran with its real outcome or a `Not run:` row with the reason. "Tests pass"
without a row is not a result. *When* the author verified something by hand
(a dashboard, a screenshot, a production query), it is a row prefixed `author:`.

**Risk and rollout** — *when* the change adds a migration, an env var, a
dependency, a hook stage, a scheduled job, or changes behavior on a production
path: what could break, how a reader would notice, and how to roll back. Steps
every teammate must take after merging ("re-run `pre-commit install`") go here.

**Review focus** — one line: where to start reading and what kind of feedback is
wanted (design, correctness, naming, test coverage). This is the rarest element in
real PRs and the strongest predictor that they get reviewed and merged.

**Follow-ups** — *when* the work surfaced out-of-scope problems: one line each,
stated as facts, not promises.

**Links** — *when* any exist: `Closes #N` (the exact keyword auto-closes on
merge), the tracker card, the spec, related PRs.

Footer — the attribution line the repo or the user's settings ask for, if any.
*When* a dispatch runner opens the PR, the footer names it: `Opened by agent:<runner>`.

## Verification rules

- Lint every touched source file with the repo's linter; run the repo's hermetic
  test selector for the touched areas. Never live tests and never the full suite
  unless the change is repo-wide or the author asks.
- *When* the pre-commit config changed: `pre-commit run --files $(git diff
  --name-only origin/<base>...HEAD)`. Never `--all-files` on a shared checkout;
  autofix hooks rewrite untouched files, and restoring them is work the reviewer
  should not have to trust.
- A tool that cannot run on this machine (no virtualenv, no local database, no
  package manager) becomes a `Not run:` row naming the missing tool. It is never
  skipped silently.

## Write it, then open or update the PR

1. Write the body to a file in the session's scratchpad or temp directory
   (`pr-<branch>.md`), or the path the author names. Never paste the body back
   into the conversation.
2. If the branch is not on the remote, push it (`git push -u origin <branch>`,
   never `--force`). If hooks or settings block the push, stop and report that
   the branch needs pushing.
3. New PR: `gh pr create --base <base> --title "<title>" --body-file <path>`
   (add `--draft` when verification is incomplete).
   Existing PR: `gh pr edit <number> --body-file <path>`; the title changes only
   if the scope of the branch changed.
4. Return the PR URL and the body path. Nothing else.

## Common mistakes

| Mistake | What it looks like | Fix |
|---|---|---|
| Invented motivation | "This was needed because every request reloaded the config" (not stated anywhere) | The why is quoted from a commit body, issue, card, spec, or the author |
| Suggested checks instead of run ones | "Sanity check: the migration should apply cleanly" | Run it; the table holds the real outcome |
| Claims from `--stat` | "only comment changes outside `src/`" without opening the diff | Read the diff before making any "only"/"no" claim |
| Diff narration | A table of before/after paths that restates `git diff --stat` | Group by intent; a reviewer can read the stat themselves |
| File inventory | "What changed" lists every renamed and edited path | Eight bullets by intent; the reviewer has the file list |
| Buried breaking change | A new required env var mentioned in passing | `!` in the title, `BREAKING CHANGE:` line, rollout step |
| Hidden incidental change | An autofix or drive-by removal absent from the text | One explicit line under What changed |
| Restating the commit body verbatim | Summary identical to `git log -1 --format=%b` | The commit explains one commit; the PR explains the branch to a reviewer |
