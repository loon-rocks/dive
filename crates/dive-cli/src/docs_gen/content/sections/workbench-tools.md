---
summary: Tools only the workbench's own agents can call, to act on what the workbench has open.
---

- The `*_artifact` tools work the same way on plans, agents and tools: list, show, open, draft, run, save.
- The plan edit tools change one part of the open plan draft at a time.
- Plan steps, `dive chat`, `dive ask` and MCP clients can't call them. Outside the workbench, edit plans with [`dive plan set` and `dive plan step`](/reference/cli/plan), or the `dive_plan_*` [tools of `dive mcp serve`](/integrating/mcp/reference/server-tools).
