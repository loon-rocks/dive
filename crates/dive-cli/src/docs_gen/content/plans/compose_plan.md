---
summary: Drafts a whole plan for a goal from the tools in your catalog.
fields:
  needs:
    type: object
    description: What the goal requires, as `inputs`, `capabilities` and `details`, worked out before drafting.
  plan:
    type: object
    description: The drafted plan. Null when no tool could do part of the goal.
  valid:
    type: boolean
    description: Whether the drafted plan passed validation.
  problems:
    type: string[]
    description: Validation problems left after fixing. Empty when the plan is valid.
  yaml:
    type: string
    description: The drafted plan as a YAML file.
  questions:
    type: object[]
    description: Values the draft had to guess, as questions for you, each with the guess it used.
  missing:
    type: object[]
    description: Capabilities no tool in the catalog could cover. Empty when a plan was drafted.
---

## How it works

1. Lists the plan's inputs and the capabilities it needs.
2. Searches the catalog for a tool for each capability. If none fits, it stops and returns what's `missing`.
3. Writes the whole plan in one call.
4. Asks you about values it had to guess.
5. Validates the plan and fixes failing steps, up to three rounds.

`dive plan draft`, the workbench and MCP's `dive_plan_draft` all run this plan.
