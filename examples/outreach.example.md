# Outreach directory

Copy to `~/.config/claude-harness/outreach.md` and replace every placeholder.
The `outreach` skill reads this file on every contact. Everyone below is
fictional.

## Me

- Name: Sam Example (Software engineer)
- Slack: U0000000001 · Email: sam@example.com
- Reach me for escalations: Slack DM to myself (draft), then email draft.
- Time zone: America/New_York · Hours: Mon–Fri 09:00–18:00

### Voice

- Direct, friendly, no filler. First line is the ask.
- Plain English, short sentences, no emoji.
- Sign-off: none on Slack; "— Sam" on email.
- Never say "as an AI" or mention the harness unless asked.

## People and roles

| Person | Role | Owns questions about | Medium | ID | Time zone / hours |
|---|---|---|---|---|---|
| Alex Rivera | Tech lead | architecture, scope, priorities | Slack DM | U0000000002 | America/New_York, 09:00–17:00 |
| Priya Natarajan | Product manager | requirements, acceptance criteria | Slack thread on the card channel | U0000000003 | Europe/London, 09:00–17:30 |
| Jordan Lee | Platform engineer | CI, infrastructure, access | Slack channel #example-platform | U0000000004 | America/Los_Angeles, 10:00–18:00 |
| Morgan Blake | Engineering manager | blockers older than 2 days, cross-team | Email | morgan@example.com | America/New_York, 09:00–17:00 |

## Channels

| Channel | ID | Use for |
|---|---|---|
| #example-app | C0000000001 | card questions and status replies |
| #example-platform | C0000000002 | CI and infrastructure requests |

## Escalation order

1. The person who asked, or the card's owner/requester.
2. The role owner above for that kind of question.
3. Alex Rivera (Tech lead).
4. Me (always last; draft).

Morgan Blake only when a blocker is older than 2 working days, and always as a draft.

## Follow-up cadence

- One follow-up in the same place after 1 working day without an answer.
- Then the next step in the escalation order, linking the original ask.
- Never more than one open ask per person per card.

## Auto-send vs always draft

Auto-send (still subject to the runner's `[send]` policy):

- Status replies in #example-app about cards and PRs, with links.
- Clarifying questions to Alex Rivera and Priya Natarajan about a card's spec.
- Follow-ups on questions already asked in the same thread.

Always draft:

- Anything by email.
- Anything to Morgan Blake or to people outside this file.
- Anything committing me to dates, estimates, money, scope or priorities.
- Opinions on people, disagreements, or declining a request.

Commitments the harness may make without asking: none.
