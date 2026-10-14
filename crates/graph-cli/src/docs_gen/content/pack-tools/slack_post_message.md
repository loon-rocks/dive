---
summary: Posts a message to a Slack channel or thread.
---

## Behavior

- Returns the message's `ts`. Pass it as `thread_ts` in a later call to reply in that thread.
- A post Slack refuses, such as an unknown channel or a missing scope, fails the step.
- `text` uses Slack mrkdwn, not GitHub markdown.
- Has side effects.
