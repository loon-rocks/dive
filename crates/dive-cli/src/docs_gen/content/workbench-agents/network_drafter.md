---
summary: The drafting architect. Interviews you, sketches the most efficient network of tools, plans and agents, has the builders draft each node, tests the network and saves it when you say so.
---

- Starts when you run `dive wb agent`, `dive wb tool`, or `dive wb plan` without naming a plan. `front_desk` hands you to it whenever you want to build something.
- Asks one question at a time, each with its guess and its confidence, and ends the interview with a restate you confirm.
- Designs for small models, composed plans, decide and route gates, map/reduce and concurrency, and pushes back once when you ask for something more expensive than the job needs.
- Uses a decision model for every classification, yes/no check, choice and score when a `decider` role is configured (`decide` gates, `builtin__decide`, decision tools), and `infer` on the `judge` role only when none is. Its model roles section says which.
- Runs `tool_drafter`, `plan_drafter` and `agent_drafter` as subagents, one per node, all at once.
- Tests the network as soon as it's built, then asks whether to save it.
- Saves the whole network at once, after a test run passes.
