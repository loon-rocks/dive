---
summary: Writes a new tool with you, test-runs it, and saves it to the project.
---

## How it works

1. Checks the catalog first, and points you to an existing tool if one already does the job.
2. Writes the tool file to the same design principles as the workbench's architect, and shows it to you. With a `decider` role configured, it writes a decision tool rather than a prompt tool to classify, choose or score.
3. Test-runs it once, after you approve. A secret the run needs is asked for in the form, for that run only.
4. Saves it to `.dive/tools/` after you confirm. You can use it right away.

In the workbench, use `dive wb tool` instead.
