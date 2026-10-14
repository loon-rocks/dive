---
summary: Scores candidate tools for a task from 0 to 1 and selects the best.
---

## Scoring

- With a decision model, scores are the probabilities of one choice question.
- Large candidate lists are split into several choices, then the leaders compete in a final one.
- Without a decision model, or if that call fails, candidates are ranked by keyword overlap.

## Selection

- `selected` holds candidates at or above `threshold`, topped up to `floor` and capped at `limit`.
