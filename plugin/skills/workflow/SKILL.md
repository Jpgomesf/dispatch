---
name: workflow
description: Entry point for claude-harness runs. `heartbeat` triages Slack and the issue tracker since the last cursors and returns a HeartbeatResult; `card <ref>` works one tracker card end to end (plan, implement, verify, review, PR, report) and returns a CardResult. Invoked by the harness runner with a JSON context block.
argument-hint: "heartbeat | card <ref>  (followed by a JSON context block)"
disable-model-invocation: true
---

# Workflow

You run unattended. Nobody answers questions in this session: anything you need
from a person goes through the `outreach` skill, and the run ends with exactly one
JSON object (the result shape below). No prose after it.

Arguments: `$ARGUMENTS`

## 0. Parse the context

The prompt holds a mode (`heartbeat` or `card <ref>`) and a JSON context block,
either inline in the arguments above or right after the invocation. Parse it
first; if it is missing or invalid, return the failure result for the mode
(heartbeat: unchanged cursors, empty lists, `summary` naming the problem; card:
`status: "failed"`).

- heartbeat: `{now, cursors, sources, workspaces, outreach_file}`
- card: `{now, ref, workspace, outreach_file}` (`workspace` may be null)

`now` is the clock for every time decision (lookback, working hours, follow-ups).

## Rules for every run

- **Tools are discovered, never assumed.** Before touching a source, find its
  tools (use ToolSearch when tools are deferred): Slack (`slack_read_channel`,
  `slack_read_thread`, `slack_search_*`), the tracker named in `sources.tracker`
  or implied by the ref (Linear: `list_issues` / `get_issue` / `save_comment`;
  Jira / Atlassian: JQL search / get issue / add comment; GitHub Issues or
  Projects: the `gh` CLI). If no tool reaches a source, say so in `summary` and
  leave that source's cursor unchanged.
- **Guardrails.** Never force-push, rewrite pushed history, push to the default
  branch, delete branches, files, cards, messages or data outside the change you
  are making, or run write statements against a database. Never merge a PR.
- **Sends go through `outreach`.** Never call a messaging send tool directly. A
  runner denial ("create a draft instead") means draft; see `outreach`.
- **Budget.** The runner caps spend per run. Keep the main context lean: delegate
  reading and searching to subagents and keep their conclusions, not their dumps.
  When the work is clearly larger than one run, finish a coherent slice, open the
  PR for it, and list the remainder in the card comment.
- **Model tiers for subagents:** `haiku` for quick searches and lookups, `sonnet`
  for review and moderate implementation, `opus` for planning and complex or
  cross-cutting changes.

## Heartbeat

Cheap triage. No code work in this mode.

1. **Read since cursors.** For each Slack channel in `sources.slack_channels`
   (plus threads and DMs addressed to the user, when the tools expose them) and
   for the tracker via `sources.tracker_query`, read items newer than
   `cursors["<source id>"]`. Source ids: `slack:<channel id>`, `tracker:<tracker>`.
   Cursor value: the newest item's timestamp (Slack `ts`, tracker `updatedAt`),
   opaque to the runner. No cursor: look back 24h from `now`.
2. **Classify each item** (skip the user's own messages and your earlier drafts):

   | Class | Examples | Action |
   |---|---|---|
   | quick reply | status question answerable from the tracker, a PR or a repo, cheaply and verifiably | `outreach` reply → `replied` or `drafted` |
   | needs card | tracker card matching the query whose body is a spec | add ref to `cards_to_work` |
   | needs card, no spec | card without a usable spec; Slack request needing real work | `outreach` asks for a spec or a card → `drafted` / `replied` |
   | blocked-on-me | review request, decision or question only the user can answer | `outreach` escalates to the user → `escalated` |
   | unblock | an answer to a question you asked on a card marked blocked | add that ref to `cards_to_work` |
   | FYI | announcements, bots, chatter | `ignored` |

   When unsure between quick reply and blocked-on-me, choose blocked-on-me.
3. **Advance cursors** only past items you handled. If a source failed mid-read,
   keep its old cursor. Return every cursor you received, changed or not.
4. **Return** exactly:

```json
{"cursors": {"slack:C0000000000": "1700000000.000100", "tracker:linear": "2026-01-01T00:00:00Z"},
 "handled": [{"source": "slack:C0000000000", "item": "<permalink or id>", "action": "replied"}],
 "cards_to_work": ["EX-123"],
 "summary": "3 items: 1 replied, 1 escalated, 1 card queued"}
```

`action` is one of `replied | drafted | ignored | escalated`.

## Card

Pipeline for `<ref>`. Each step names its stop condition.

1. **Read the card** with the tracker tools: body (the spec), comments, links,
   attachments. Check for existing work first: a branch or open PR mentioning the
   ref means resume, not restart.
2. **Resolve the workspace.** Use `workspace` from the context; if null, match
   the ref prefix, team or repo named in the card against `match` in the
   `[[workspaces]]` of the runner config (`$HARNESS_CONFIG`, default
   `~/.config/claude-harness/config.toml`; read only). No match, or the path
   is not a git checkout → `blocked`, `blocked_on: "no workspace for <ref>"`.
   Read the workspace's `CLAUDE.md` / contributing docs: their rules win.
3. **Gap check.** List what the spec leaves open. Decide what you can default
   safely (naming, internal structure) and record those defaults for the PR.
   Anything that changes behavior, interfaces, data or scope and is not in the
   spec → ask via `outreach` (one message, all questions, proposed defaults),
   comment the questions on the card, return `blocked`.
4. **Branch.** From the up-to-date default branch: `harness/<ref-lowercase>-<slug>`
   (or the workspace's own convention). Small Conventional Commits; run the
   workspace's lint before each commit.
5. **Plan** (opus subagent for anything beyond a single-file change): tasks,
   files, tests per task, and which tasks are independent.
6. **Implement.** Dispatch independent tasks to parallel subagents (sonnet by
   default, opus for complex ones), dependent tasks in order. Tests first where
   the workspace has tests. Each subagent reports files changed and checks run.
7. **Verify** with the workspace's own definition of done: `CLAUDE.md`, then
   `make`/`package.json`/`pyproject.toml` targets for typecheck, tests and lint.
   Run them yourself; keep the real output.
8. **Review:** run `/code-review` on the branch diff; optionally `/simplify`, and
   `/security-review` when the change touches auth, secrets, input handling or
   infrastructure. Fix findings you agree with; note ones you reject and why.
   Re-run step 7 after fixes.
9. **Stop conditions.** Two failed fixes for the same failure (check, review
   finding, or runtime error) → stop: push the branch, open the PR as a draft
   (`gh pr create --draft`) with the failure in the body, escalate via `outreach`,
   return `blocked`. A required check that cannot run on this machine → draft PR,
   `blocked_on` names the missing tool.
10. **PR.** Push the branch (never `--force`), write the body with the
    `pr-description` skill, `gh pr create` (or `gh pr edit` on resume). Link the
    card (`Closes`/ref per tracker convention).
11. **Report on the card:** comment with the PR link, one-line summary, defaults
    you chose, and anything left over. Move the card to the tracker's review
    state when one exists. Never close or delete it.
12. **Return** exactly:

```json
{"ref": "EX-123", "status": "done", "pr_url": "https://github.com/example-org/example-app/pull/1",
 "blocked_on": null, "summary": "Added CSV export endpoint; PR open, checks green"}
```

`status`: `done` (PR open, checks green), `blocked` (waiting on a person or a
missing prerequisite; `blocked_on` says who or what), `failed` (unexpected error
you could not route to a person). `pr_url` is set whenever a PR exists, including
draft PRs.
