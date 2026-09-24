---
name: outreach
description: Use whenever you must contact a person on the user's behalf — reply to a message, answer in a thread or on a ticket the user takes part in, ask a clarifying question on a card, escalate a blocker, tell the user about a card dispatch stopped retrying, or follow up. Decides whom to contact, on which medium, send vs draft, the message shape, follow-up cadence and escalation order from the user's outreach directory file.
argument-hint: "<what is blocked or needs a reply> [card/PR/thread link]"
---

# Outreach

Get the user's work unblocked without embarrassing the user. You write as, or on
behalf of, the user; the outreach file is the only authority on who, where, when
and what may be said.

## 1. Load the directory

Read `outreach_file` from the run's JSON context (default
`~/.config/dispatch/outreach.md`). It defines: the user and their voice,
people and roles (with IDs and preferred medium), channels, escalation order,
working hours and time zones, follow-up cadence, and what may be auto-sent vs
always drafted.

File missing or unreadable → draft only, addressed to the user, and say so in
the result.

## 2. Decide

1. **Whom.** In order: the person who asked (reply where they asked); the owner
   or requester of the card; the role owner the file names for this kind of
   question; the user ("me"). Never contact someone absent from the file except
   by replying in a thread where they addressed or mentioned the user.
2. **Duplicates.** Right before writing, reread the thread, the card comments and
   your recent drafts. If the user, you, or another runner (the user's account,
   or a message tagged `agent:<name>`) already answered after the triggering
   message, stop: report `skipped`. If your ask is already out and unanswered,
   this is a follow-up (step 6), not a new message.
3. **Medium.** The person's preferred medium from the file; a thread reply when
   the question came from a thread; a card comment when the question belongs to
   the card's record (a card comment is fine alongside a message, not instead of
   a denied one).
4. **Send or draft.** Send only if the file marks this kind of message as
   auto-send for this person or channel **and** `now` is inside both the user's
   and the recipient's working hours. Otherwise draft. Anything that commits the
   user to dates, money, scope, priorities or opinions on people is always a
   draft unless the file explicitly allows it for that recipient.

## 3. Message shape

Short, in the user's voice from the file. Four to six lines:

- the specific ask, first line, answerable in one reply;
- the context link (card, PR, thread);
- what is blocked until it is answered;
- the default you will take if there is no answer by a stated time (only a
  default the file or the spec allows).

An answer to someone's question has the same length but a different order: the
answer first, then the evidence (links, `file:line`, command output), then what
was not verified.

No apologies, no filler, no promises the file does not allow. One message holds
all open questions for the same person.

## 4. Threads you do not own

When the user takes part in a thread (a Slack thread, or comments on a ticket
assigned to someone else) and is mentioned or replied to there:

- Reply **in that thread**: a Slack thread reply, or a tracker comment answering
  the specific comment (as a reply to it when the tracker supports threads,
  otherwise quoting or @-mentioning its author). Never start a new thread, DM or
  channel post for it unless the asker asked for that.
- Answer only what was asked, with evidence (links, `file:line`, command
  output). Do not restate the ticket or give unrequested advice on it.
- Never change the ticket: no status, assignee, labels, estimate or claim
  comment, and never open a branch or PR for it. Those belong to its owner.
- If someone asks the user to do the work, offer to take it once it is assigned
  to the user; that offer commits the user, so it is always a draft.
- Addressing the asker by replying is allowed even when they are absent from the
  outreach file; send vs draft still follows "Send or draft" in step 2.

## 5. Send, and handle denial

Use the messaging tools available (discover them; do not assume a vendor).
Whether a send is allowed is decided by the user's Claude Code permissions:
`permissions.deny` / `ask` rules and any `PreToolUse` hooks they configured.
dispatch never gates sends. If a send is denied — a permission denial, a
rule that would need an unavailable approval, or a hook blocking the call,
whatever its wording:

- create the same message with the equivalent draft tool (Slack
  `slack_send_message_draft`, Gmail `create_draft`, or the tool's draft variant);
  when the tool has none (most tracker comments), draft it to the user instead,
  stating where it was meant to go, with the link;
- do **not** retry the send, switch channel or medium, schedule it, post it as a
  tracker comment, or deliver it through a CLI or HTTP call;
- record the action as `drafted`.

Never delete, edit or unsend someone else's messages or your earlier ones.

## 6. Follow-up and escalation

Use the file's cadence; if it states none: one follow-up in the same place after
one working day without an answer, then move one step down the escalation order
with a link to the original ask. The last step is always the user ("me"), as a
draft or self-message that states what is blocked and the proposed default.
Follow-ups due outside the recipient's working hours wait for a later triage
inside them.

## 7. Report back to the caller

One line per contact: `person | medium | sent|drafted|escalated|skipped | link`.
The workflow maps `sent` to `replied` (TriageResult and DiscussionResult),
`skipped` to `ignored` in a TriageResult and to `skipped` in a
DiscussionResult.
