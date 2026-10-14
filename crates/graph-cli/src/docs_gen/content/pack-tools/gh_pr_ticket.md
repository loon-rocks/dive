---
summary: Finds a ticket identifier, such as `LN-1234`, in a pull request.
---

## Behavior

- Checks the head branch, then the title, then the body; the first match wins.
- Normalizes to uppercase with a dash.
- No ticket returns `found: false`, not an error.
