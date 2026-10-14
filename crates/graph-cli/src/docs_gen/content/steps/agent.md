---
summary: Hands one step to a model that calls tools until it can return structured output.
fields:
  output: The answer, matching `outputSchema`.
  iterations: How many model rounds the step took.
  tools_called: Each tool call, with the round it happened in.
  tools_called.[].tool: The tool's name.
  tools_called.[].round: The round the call happened in.
  final: False when the round limit ran out before a complete answer.
---

## When to use it

- When which tool to call next depends on what an earlier call returned.
- When the order of calls is known in advance, write plain steps instead. They make no model calls.

## How it works

- `tools` limits what the step can call, by name or glob. Omit it for the whole catalog.
- `maxIterations` caps model rounds. The default is 8.
- `systemPrompt` is added after the built-in prompt.
- Each round is a model call, and inside `map` it runs once per item.
