---
name: docs-guides
description: Write or revise a how-to guide under docs/*/guides/ (one task, done end to end). Use when drafting a new guide page, tightening an existing one, or turning an outline stub into a guide.
---

# Writing docs guides

A guide walks the reader through one concrete example, built and run for real, and
nothing else. Everything on the page earns its place by moving the reader toward
that example's result; the rest lives on reference and overview pages, or in
another guide.

## Page shape

~~~mdx
---
title: "Build a multi-agent network"
description: "Build a triage agent that hands release questions to release_notes, which asks a reviewer subagent about risky changes."
keywords: ["subagents", "handoffs", "transfer_to", "agent network"]
---

Agents connect in two ways:

- A handoff moves the conversation to another agent.
- A subagent does one job for its caller and returns the result.

## <Build the first piece>

`triage` lists … under `handoffs`:

```yaml .graph/agents/triage.yaml
…
```

## <Run it>

```bash
graph ask --agent triage \
  "Draft release notes for what is unreleased."
```

```text
<real output from running it>
```

## Related guides

<CardGroup cols={2}>
  <Card title="…" icon="…" href="/…">
    One line on what that guide does.
  </Card>
</CardGroup>
~~~

The opening is optional. Include it only when the reader needs a distinction to
follow the steps, and write it as a short bullet list, not a paragraph.

## Rules

- **The sub-head names the concrete example.** `description` says what the reader
  builds, by name, and what it does: "Build a triage agent that hands release
  questions to release_notes, which asks a reviewer subagent about risky changes",
  "Extend the built-in chat agent with a new subagent for researching things on
  the internet". Not a generic capability ("Connect agents with handoffs and
  subagents"). Every verb in it must be something the guide shows; if the product
  can't do it (e.g. test-run an agent draft), it isn't in the sub-head.
- **No intro that restates the sub-head or explains the mechanism.** Cut "This guide
  builds…" and lines like "An agent file with the same name as a built-in replaces
  it." The mechanism shows up in the step that uses it, in one clause.
- **Only the goal.** Never introduce features outside the example, not even as a
  one-line aside ("to change an existing agent instead…", "you can also draft from
  chat…", the home-directory variant of a project file). Adjacent tasks go in
  Related guides.
- **Concise and punchy.** Cut limits, caps, internals, alternative paths and
  nice-to-know details ("at most two questions at a time", "nesting is capped at 3",
  `wait_for_user`, a second way to save, how to discard). If the reader can reach
  the result without the sentence, delete it.
- **Section intros name their subject.** "List the agents `triage` can hand the
  conversation to under `handoffs`", not "List the agents it can…".
- **The example's file goes in the side panel.** Put the file the guide works with
  in a `<RequestExample>` after the opening line, with its path as the block title,
  as overview pages do; steps refer to it by name. Files the guide writes step by
  step, or edits between runs, stay inline in their steps.
- **No Before you start / Prerequisites / Check it worked sections.** Outline stubs
  carry these headings; drop them. Anything the example needs (an MCP server, a
  tool, a plan) is a step of its own. Verification goes inline: the command, then
  its real output, right after the step it checks.
- **Screenshots go inside the step they illustrate**, never in a gallery or at the
  end. Workbench shots are generated, never mocked: use the `workbench-shots` skill.
  The scripted conversation in a shot must match the guide's story and example names.
- **End with `## Related guides`**, a `<CardGroup cols={2}>` pointing at the next or
  neighbouring tasks. Card titles match the target page's `title` exactly. This is
  where readers learn about everything the guide left out.

## Build the example for real

- Before writing, build the example's files in a scratch project (a scratchpad dir
  with its own `.graph/`) or, when it needs this repo's git history, temporarily in
  the worktree's `.graph/` — and delete those files afterwards.
- Validate every file you show (`graph plan validate`, `graph agent validate`)
  and run the example (`graph ask … </dev/null`, `graph plan run`) with the debug
  build. Pass `</dev/null` to `graph ask`, or it waits on stdin.
- Output blocks are copied from those runs. Trim long lines with `…`, never invent
  lines or numbers.
- If a run shows the guide's promise doesn't hold (cost went up, a tool doesn't
  exist), change the example until it does, and tell the user what you found.

## Writing details

- Current behavior only. Verify every claim against the code (agent prompts in
  `crates/graph-cli/src/workbench/agents/`, tool implementations, clap help) before
  writing it; when the product can't do something the guide implies, flag it to
  the user instead of writing around it.
- No process content: no draft banners, TODO lists, review notes or source lists on
  the page.
- Use full command names (`graph workbench`, not `graph wb`).
- Every code block has a language (`bash`, `yaml`, `toml`, `json`, `text`,
  `handlebars`); file contents carry their path as the block title
  (```` ```yaml .graph/agents/triage.yaml ````). Excerpts of a longer file use `…`.
- Wrap long commands with `\` so they fit a phone screen:

  ```bash
  graph ask --agent release_notes \
    "What shipped since v0.15.0?"
  ```

- Keys use `<kbd>`: <kbd>Ctrl</kbd>+<kbd>S</kbd>.
- Root-relative links (`/plans/overview`), and link the first mention of a concept
  to its overview.
- Reuse example names across guides (`release_notes`, `unreleased_changes`,
  `reviewer`) so the guides read as one story.
- Facts shown on more than one page come from generated snippets
  (`docs/snippets/generated/`), never copied text.

## Check before handing it over

1. `mint validate` and `mint broken-links` from `docs/`.
2. Re-read the page against the rules above, sentence by sentence, asking whether
   each one moves the reader toward the sub-head's result.
3. In the reply, list what was dropped from the outline stub and why, so the user
   can pull anything back.
