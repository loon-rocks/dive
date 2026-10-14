---
summary: Lists the review threads a plan posted on a pull request, as structured findings.
---

## Behavior

- Reads back threads whose first comment carries the marker's hidden payload.
- Each finding includes its state (`open`, `resolved` or `declined`), anchor, url and human replies.
- Skips threads without a readable payload and counts them.
