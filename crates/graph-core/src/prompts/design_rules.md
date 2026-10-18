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

Treat context as scarce memory and tokens as clock cycles: every token read costs time and money, every token written costs several times more, and a long-lived context is paid for again on every round. Apply the patterns of a constrained system:
- Index, then seek: read a table of contents (a file list, hunk headers, per-file match counts, names and one-line descriptions) and fetch only the slices it points to, by line window, path or anchor. Never scan a whole file, diff or page to find one part.
- Track what changed: record a watermark (a sha, a timestamp, an id) when work is done, and next time process only the delta since it.
- Cascade: a comparison or a command settles what it can, the decision model takes what's left, and a chat model only the remainder that needs prose.
- Never copy data through a model: a model returns ids, verdicts and the new text only, and reshapes join them back to the data. A decision model's output is a probability, so it writes nothing.
- Isolate and pass references: work whose result the caller needs only a few lines of (searching, reading, research) runs in a subagent's own context, on a small model, and returns facts with pointers (path and lines, URL and anchor), not copies. The caller dereferences a pointer only when it needs the exact text.
- Load lazily: a long-lived agent keeps the index; detail loads where it's used, in a builder's or subagent's short-lived context.
- Shorten the critical path: fan independent items out with map and concurrency, put independent tool calls in one response, and end early with exit gates. Each serial model round adds its full latency.
- Fixed before variable: what stays the same across calls (instructions, reference text) goes in the system prompt (a prompt tool's `system`, an agent's system_prompt) and what changes (the item) in the message, so the prompt cache reuses the fixed part.
- A longer timeout, a bigger byte budget or a lower cap only hides the cost; a faster model alone keeps the same shape.
