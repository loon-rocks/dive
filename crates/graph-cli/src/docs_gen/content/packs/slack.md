---
summary: Post messages to Slack channels and threads.
---

## Requirements

- `curl` and `jq` on your `PATH`.
- `SLACK_BOT_TOKEN` in the environment.

## Setup

- Create a Slack app with the `chat:write` bot scope. The pack's `app-manifest.json` has this configuration.
- Install the app to your workspace.
- Invite it to each channel you post to: `/invite @your-app`.
- Export its bot token (`xoxb-…`) as `SLACK_BOT_TOKEN`.
