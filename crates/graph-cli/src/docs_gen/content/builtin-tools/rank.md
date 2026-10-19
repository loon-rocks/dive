---
summary: Ranks a whole list against a question in one decision-model call.
---

## Behavior

- The items become the options of one `choice` question, `state` is the context they're judged against, and `instructions` is the question.
- Each option shows the item's `describe` field, or the whole item when `describe` is absent.
- `items` is the list sorted by probability, highest first, and `ranked` pairs each item with its probability. Both are cut to `top_k`.
- Options are shown in a shuffled order seeded from the items, since decision models lean toward the first options. The same list ranks the same way on a rerun.
- `none` adds a "nothing fits" option. Its probability comes back as `none`, and `no_fit` is true when it beats every item.
- Lists over 254 items are ranked in chunks sent together, and each chunk's best meet in a final choice. Items that don't reach the final choice follow the finalists with a `null` probability.
- Needs a decision model role, `decider` by default.

## When to use it

- Reranking search results, candidates or options against a question: one call instead of one per item, and no LLM.
- Keep `state` to what the question needs: the question and its context, not the items.
- For an absolute yes or no per item rather than an order, use a `filter` step with a `decide` gate.
