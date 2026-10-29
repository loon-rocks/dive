A user tool is one YAML file. Once saved, it's called as `user__<name>` by agents, plan steps and the CLI.

Top-level keys:
- `version`: the file version; leave it out and Dive stamps the current one.
- `name` (required): letters, digits, `_` and `-` only. Short and specific: `weather_now`, `gh_pr_checks`.
- `description` (required): what the tool returns and when to use it. Planners pick tools by this text, so name the system, the input and what comes back ("Current weather for a city from wttr.in: temperature, sky, wind").
- `kind` (required): `exec`, `prompt`, `reshape` or `decision`.
- `input_schema`: a JSON Schema object. Give every property a `type` and a `description`, and list the ones that must be present in `required`. The input is checked against it before every call.
- `output_schema`: a JSON Schema for the result. Only declare it when you know the shape; a test run records the observed shape anyway.
- `read_only`: `true` when the tool only reads (fetches, lists, computes), `false` when it writes, sends, posts, deletes or changes anything. Be honest: plans and agents treat a tool that isn't read-only with more care.

Templates in a tool reference only its input: `{{input.city}}`, `{{input.repo}}`. Nothing else exists inside a tool.

`exec` runs a command:
- `command` (required): the executable, such as `curl`, `gh`, `git`, `jq`, `python3`.
- `args`: a list of arguments, each a string that may contain `{{input.*}}` templates. Each list entry is one argument, so no shell quoting is needed, and there is no shell: pipes and `&&` need `command: bash` with `args: ["-c", "<script>", "--", "{{input.x}}"]`, reading the input as `$1`.
- `env`: extra environment variables; values may use `${VAR}` from the environment Dive runs in. A value is a string, or `{value, description}` when it needs explaining.
  - A setting that differs between machines, such as a self-hosted service's URL, gets a default with `${VAR:-default}`: `SEARXNG_URL: "${SEARXNG_URL:-https://search.example.com}"`. It works as written and another machine can override it. The default must be a value the user gave you: when you don't know their URL, host, account or ID, ask them. Never fill one in with a public service, a demo host or a placeholder.
  - A secret, such as an API token, is referenced with no default, and always gets a `description`: markdown saying what the value is (with the scopes or permissions it needs) and where to get it, with a link, taken from the docs you read. The user sees it when they're asked to set the secret.
    ```yaml
    HUBSPOT_TOKEN:
      value: "${HUBSPOT_TOKEN}"
      description: "HubSpot private app token with `crm.objects.contacts.read`. Create one under [Settings › Private apps](https://app.hubspot.com/private-apps)."
    ```
    Never put a secret's value in the file, and never ask the user for one in the conversation: the value is never passed to you.
- `cwd`: the working directory.
- `timeout_secs`: default 60.
- `output`: `json` (the default) parses stdout as JSON; `text` wraps stdout as `{"text": …}`. Prefer JSON: ask the command for JSON (`gh … --json`, `curl` against a JSON API, `jq -c`).
- Files in: an input property with `format: blob` (or an array whose `items` have it) takes a file ref such as `blob:sha256-….png`, and the command receives it as a local path. Declare every input that takes a file this way; any other input gets the ref as plain text.
- Files out: declare each file the command writes under `files` at the top level of the tool, `files: {report: {name: report.pdf}}`, and pass `{{files.report.path}}` in `args`; for several, declare a pattern such as `page-*.png` and pass `{{files.pages.dir}}`. When the command exits 0, each file is listed under the result's `files` with its `name`, `ref`, `mime` and `bytes`; with `output: json`, stdout may then be empty. Never print a file's bytes or base64 to stdout.
A non-zero exit, a timeout, or `json` output that doesn't parse comes back as a tool error with stderr.

`prompt` makes one model call:
- `prompt` (required): the instruction, with `{{input.*}}` templates.
- `system`: a fixed system prompt.
- `model`: a model role, default `chat`.
- `output_schema` (top level) makes the result structured JSON; without it the result is `{"text": …}`.
Use it for judgment over text the caller passes in: classify, extract, summarize. A file ref in the input reaches the model as the image when its role takes images, so a prompt tool can also describe or check a screenshot.

`reshape` copies and renames fields of its input with `shape`, a template tree over `{{input.*}}`. It computes nothing.

`decision` asks a decision model typed questions with `state` and `questions`: a likelihood, a choice among options or a score. When a decision model is available, use it instead of a `prompt` tool to classify, choose or score.

Example:

```yaml
name: weather_now
description: Current weather for a city from wttr.in, with temperature in Celsius, conditions and wind.
kind: exec
command: curl
args: ["-sf", "https://wttr.in/{{input.city}}?format=j1"]
read_only: true
input_schema:
  type: object
  required: [city]
  properties:
    city: {type: string, description: "City name, such as Denver"}
```

An exec tool that returns a file:

```yaml
name: page_screenshot
description: A full-page PNG screenshot of a web page, returned as a file ref under files.
kind: exec
command: bash
args: ["-c", "npx -y playwright screenshot --full-page \"$1\" \"$DIVE_OUT/page.png\" >&2", "--", "{{input.url}}"]
timeout_secs: 120
read_only: true
input_schema:
  type: object
  required: [url]
  properties:
    url: {type: string, description: "The page to capture, such as https://example.com/pricing"}
```
