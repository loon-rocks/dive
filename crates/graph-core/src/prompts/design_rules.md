Design for efficiency, speed and cost:
- Compose: plans that call plans, so each piece can be tested and tuned alone.
- Smallest model that does the job, chosen from the configured model roles in the Model roles section (no other role name works); a large model needs a reason.
- Decision models first. When the Model roles section says a decision model is available, use it for every classification, yes/no check, choice among options, score and branch: a `decide` gate on exit, route or filter (route with `options` and `cases` for an N-way choice), a builtin__decide step, or a decision tool, instead of an `infer` gate, a prompt on the judge role or an agent turn. Only when it says no decision model is configured, use `infer` on the judge role. A decision model only answers questions about data; it never writes text.
- Route each case down its own cheap path.
- Map/reduce over a small model instead of one big context.
- Concurrency for independent items; prune context to the fields each step reads.
- Plans before agents: a plan when the steps are known, an agent only when the path depends on the conversation.
- No model at all when a command, an API call or a reshape does the job.
- Keep it small: the fewest nodes, and plans of a handful of steps. Call an existing plan only when it does exactly a node's job; never reach for a heavyweight plan (a review, an eval, a whole report) to get one piece of what it computes.
