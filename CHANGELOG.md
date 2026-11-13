# Changelog

All notable changes to Dive. Generated from conventional commits by git-cliff.

## Dive v0.20.0 — 2026-11-13

### Added

- http tools send the run token to your app, and CLI and desktop runs can act as a dev identity

### Fixed

- dive serve's OpenAPI document gives the entries stream's after as a query parameter

## Config v8 — 2026-11-13

### Breaking

- config v8 adds [http].token_origins, [identities] and [dev].identity — config version 8 (dive config migrate)

## Tool v5 — 2026-11-13

### Breaking

- tool v5 adds kind: http, which calls an endpoint in your app with the run's token — tool version 5 (dive tool migrate)

## Dive v0.19.0 — 2026-11-13

### Breaking

- lists show each plan, agent and tool's scope, and anything that can't run here is marked with what it's missing and refused before it starts (#270) — `dive plan list --json` no longer has a `hidden` key. Plans

### Added

- dive serve runs plans over HTTP with run tokens (#274)

### Documentation

- the Dive image is public, so workflows pull it without a registry login or packages permission (#269)

### Fixed

- storage usage counts sessions and their runs, so Settings > Storage no longer shows 0 after the store 5 migration (#273)

## Config v7 — 2026-11-13

### Breaking

- each run keeps its own usage, events and identity, with an [identity] section and a serve mode (#271) — config version 7 (dive config migrate)

## Store v5 — 2026-11-13

### Breaking

- sessions replace threads, and each turn or plan run is a claimed run with a lease, fenced appends and stored cancels (#272) — store version 5. Opening an older data directory converts each thread into a chat session under sessions/, with one completed run per turn under runs/, and leaves threads/ as it was.

## Dive v0.18.0 — 2026-11-03

### Breaking

- every call a turn makes is kept as a trace span with its thread, and telemetry exports call content only when asked — telemetry.capture_content defaults to false, with no

### Added

- dive-runtime exposes storage settings ops: view, save, probe and usage for [storage], [storage.files] and the data folder (#239)
- the chat model sees attached images and images tools return (#241)
- explicit attachments, and the drafting agents are ordinary built-ins (#242)
- an architect role for network_drafter and network_editor (#244)
- every unset chat role uses default (#245)
- pre_load on an agent's tools entries (#248)
- network_editor sketches every edit before building, with a problem, change and success restate and each item marked edit, new or as is (#251)
- edit evidence can focus on one step or model call inside its caller, or cover a whole turn, and network_editor starts from the focus (#255)
- test-run approvals say what the run checks and what it can touch; safe tests run without asking (#256)
- network test approvals use the test-run detail; unasked tests are announced to the host (#257)
- role images in settings ops, models.dev input modalities, and held-back images on model calls (#260)
- MCP servers that use OAuth sign in from the browser with `dive mcp login`, for one project or every project, and say they need sign-in instead of failing to connect (#265)
- MCP servers that can't register Dive automatically sign in through a hosted client ID or an OAuth app from [mcp.<name>.oauth], and a tool call that needs sign-in names its server in the result (#266)
- files of every type become refs with their own extension, MCP text resources and links of any type are kept, builtin__save_file and builtin__read_file turn text into a ref and back, and models read text files and PDFs named in what they read
- every agent that builds tools, plans or agents learns the one way files become refs: tool results, declared files, builtin__save_file and generated images

### Documentation

- say the docs are published by the loon.rocks site from docs-live, not Mintlify

### Fixed

- the global workspace saves tools, plans and agents to ~/.config/dive (#243)
- workbench tools an agent names are loaded from the start again (#246)
- attach tools an agent names are loaded from the start, and builders can open their item (#247)
- a new plan drafted in an edit session joins the edit as a node, so agents drafted beside it can call it and one save writes both (#250)
- infer offers every chat role the model roles list names, solver included, and describe_tool gives a plan's output and sees plans drafted in the thread (#252)
- network_editor sees how a call ran, every nested plan and tool with the branch each gate took, in the evidence and in each test's steps (#253)
- images follow-ups from review of #258 (#259)
- a stdio MCP server that fails to start reports the last lines of its stderr (#261)
- images too large for a chat model are shrunk to fit (#262)
- a failed turn keeps what it did and its error in the thread (#263)
- missing structured fields are asked of the same model again, and repair calls say why they ran (#264)
- the workbench runs every plan check on drafted, opened and resumed plans, so a path into a field its tool never returns is reported
- a drafted tool's test run writes in a scratch directory instead of the project, keeps the files it declares, and says what it wrote
- images sent to chat models are held to 2576 pixels a side, so a tall screenshot beside other images no longer fails with a many-image size error
- bare pack names install from loon-rocks/packs, the official packs' new home (#268)

## Config v6 — 2026-11-03

### Breaking

- every model that reads a file ref sees the image, roles can say they take no images, and builders learn to return and pass files (#258)

## Plan v3 — 2026-11-03

### Breaking

- plan steps declare the files their tools write under `files`, by name or pattern, get a call directory as {{files.<name>.path}} and read them back as {{step.files.<name>.ref}}, and MCP servers get the project as their root

## Tool v4 — 2026-11-03

### Breaking

- exec tools declare the files their command writes under `files`, by name or pattern, pass {{files.<name>.path}} in args, and return them by name

## Store v4 — 2026-11-03

### Added

- stored files live under files/ in the bucket and the data directory (#240)

## Dive v0.17.0 — 2026-10-24

### Added

- ${NAME} references in config and tool env read the OS credential store when the environment does not set them (#184)
- graph secret set, rm, list and show manage the secrets ${NAME} references read from the OS credential store (#188)
- graph pack add, update, list, check and release work without git installed (#192)
- graph-runtime reads, edits and tests providers, model roles and prices for the desktop app (#195)
- the workbench drafts through a network_drafter architect that interviews you, sketches a network and has builder agents draft it (#197)
- graph secret list includes every name stored for this project or every project, and says when the credential store refuses a read (#198)
- the drafting architect builds every node in parallel on a drafter role, tests in the same turn, and keeps drafts as threads (#200)
- the drafting architect keeps networks small and builders get the schemas of the catalog tools a sketch names (#201)
- plain chats and drafts open as one session that hosts the thread's attachments, and graph chat --thread reopens a draft with its network
- threads belong to the project they were started in, and graph thread list lists the current project's unless given --all
- exec tools from an installed pack run with GRAPH_PACK_DIR set to the pack's directory, so a pack can ship scripts and binaries
- the drafting agents and plan_editor list the web pack's search and fetch tools, and run without them when the pack isn't installed
- the edit-session lead agent is network_editor (#213)
- the designing agents share one set of design principles that puts decision models first, and network_editor fixes the design behind a failure, offering a rework beside the patch (#214)
- list tools flatten, dedupe, sort and take lists in code, and builtin__rank ranks a whole list against a question in one decision-model call (#217)
- graph plan rm, graph tool rm and graph agent rm delete a plan, tool or agent file, reverting a built-in override and naming everything that still uses it (#221)
- every agent defers the tools its list matches and loads them through plan__search_tools, scoped to its own list, cutting chat prompt tokens per turn by 70% (#222)
- conversation agents know a tool's files arrive as refs, show them in replies as markdown images, and pass them on to other tools (#229)
- roles carry a type, and an image role on the new gemini provider draws and edits images through builtin__generate_image, metered and priced like any other model (#230)
- deleting a thread removes the stored files no other thread refers to, and the S3 download cache keeps to 512 MB by dropping the least recently used files (#235)
- saving a role writes its description, and an empty one removes it (#236)
- standard roles have built-in descriptions, and a tool's model input offers the configured roles that are described once there are two to choose between (#237)

### Changed

- graph's engine wiring and pack management move into a graph-runtime library crate (#177)
- graph chat and graph ask run on a chat session in graph-runtime that the desktop app shares
- embedders can cancel graph runs, which finish the tool call in flight and then stop
- graph thread's list, show and rm return typed results from graph-runtime (#181)
- graph plan's list, show, validate and migrate return typed results from graph-runtime (#182)
- graph config's show, path, check, migrate and init return typed results from graph-runtime (#183)
- graph tool, graph agent and graph mcp return typed results from graph-runtime (#185)
- the workbench engine reports to the TUI through a WorkbenchEvent channel instead of the reducer's own messages (#186)
- the OTLP telemetry exporter moves into graph-runtime, so the desktop app exports the same traces as graph (#187)
- graph pack add, update, remove and list return typed results from graph-runtime (#189)
- the workbench engine moves into graph-runtime behind a Workbench session the desktop app can drive (#191)
- workbench events report each agent model call and how long each agent tool call took (#204)

### Documentation

- examples and CLI help name the current release instead of v0.15.0
- the infer, decide and reshape reference pages describe what each tool returns (#174)

### Fixed

- graph-runtime saves model config into older config files by migrating them, and reports keys written into the file (#196)
- structured output works on Anthropic models that refuse a forced tool choice (#199)
- a workspace opened on the global config directory reads, saves and stores secrets in that directory instead of a nested .graph layer (#202)
- a configured pack that isn't installed loads with a warning naming graph pack add, instead of refusing the config (#203)
- prompt and decision tools meter their model calls under the model role they ran on instead of the tool's name (#206)
- a reopened draft keeps its passing test, because subagent runs are recorded and replayed at the call that started them instead of after the turn (#211)
- network_editor proposes the change you ask for when an edit starts without a failed call, instead of looking for something to fix (#215)
- workbench__describe_tool describes graph's native builtin__ tools, and edit sessions open the workbench's own agents (#216)
- saved edits stop reopening as unsaved, builders finish on the node they were started for, a draft that doesn't parse is refused, and network_editor checks callers before changing an item's output (#218)
- a structured result cut off at the model's output limit fails the step with an error naming the limit, instead of being repaired into an empty value (#219)
- a resumed conversation keeps its prompt cache across turns, because agent prompts carry today's date instead of the minute the thread was reopened (#220)
- systemone providers list their models from Typesafe's /v1/models, so pickers and connection tests work for them
- builtin__generate_image saves each image with the extension of the type the model returned, so a JPEG asked for at hero.png lands at hero.jpg (#231)
- saving a role refuses to change a saved role's type, since the agents, plans and tools that use it expect that type of model (#232)
- storing a secret replaces a Keychain value another program stored, instead of failing because the item already exists (#233)
- fetching prices fills a Gemini image model's image rate and its text rate, and finds Nano Banana models the catalogue lists only under Vertex (#234)

## Config v5 — 2026-10-24

### Breaking

- secrets can be stored for one project with graph secret set --project, keyed by a generated [project] id (config version 5) (#194)
- prices take prompt-length tiers priced per call, and can be filled and refreshed from the models.dev catalogue (#223) — config version 6 (graph config migrate)
- images and audio that MCP tools return are kept as files and passed to other tools by reference, stored on disk, in memory or in S3 (#225) — config version 7 (graph config migrate)

## Plan v2 — 2026-10-24

- Introduced with Dive v0.17.0.

## Tool v3 — 2026-10-24

### Breaking

- tool env entries take a markdown description, and workbench tests wait on missing secrets instead of running — tool version 3

## Agent v2 — 2026-10-24

### Breaking

- agent files declare the thread attachments they read with attachments — agent version 2
- edit sessions open existing agents, plans and tools in a chat thread with the call that went wrong as evidence (#212) — agent version 3

## Store v3 — 2026-10-24

### Breaking

- thread entries record each tool call's duration and each model call's tokens and model (#193) — store version 3
- tool results in threads record every model and tool call made inside them at any depth, and OTel traces give parallel and subagent tool calls their own spans (#205) — store version 4
- threads record their attachments, such as the network draft a thread builds — store version 5
- threads record the project they were started in — store version 6

## Pack v2 — 2026-10-24

### Breaking

- graph is now Dive — the binary is dive and the repo is loon-rocks/dive, while graph's directories, GRAPH_* variables, stored secrets and pack requires.graph keep working until v1 (#226)

## Lock v1 — 2026-10-24

- Introduced with Dive v0.17.0.

## Dive v0.16.0 — 2026-10-14

### Breaking

- rename llm_call events to model_call and tag each with its model kind
- workbench agents work on plans through the same artifact tools as agents and tools, with 17 workbench tools folded into 7 (#146)
- plans state how they finish with a required finish key, success exits return the plan's result shape, and failed plan calls return one standard error — plan version 2 files need `finish`; plan-tool results for exited runs and empty-data failures changed shape.
- the github and slack tools leave the binary for installable packs, and [tools].packs becomes [packs.<name>] entries (#163) — the github and slack tools are no longer part of graph.

### Added

- config version 4 adds the systemone provider type, the decider role, and context_window
- a systemone decision provider speaking the Jev wire API, routed to decision roles with startup kind checks, failover, context_window, and decision traces
- named agent files with graph agents list, show, validate and migrate
- one override model: built-in plans and pack tools are replaced by files that name them, and show reports the source
- plan drafting runs as built-in plans you can override, plan steps can call named agents, and decide and exit steps work inside any body
- threads keep every agent's history as authored entries, with an owner and an active agent
- graph chat [agent] and graph ask --agent run named agents that hand off and delegate
- the workbench runs a network of agents that draft, load and edit plans with you
- drafting offers only the tools a plan needs, found by a built-in search_tools plan that asks a decision model when one is configured
- compose_plan drafts a whole plan in one call from the tools search finds, stops when no tool can do part of the goal, and fixes only the steps that fail validation
- builtin__reshape accepts a single template string and returns that value, and compose_plan rejects references to fields a tool's declared or observed output doesn't have
- map steps take onError: skip, leaving failed items out of results and listing them in failed instead of failing the plan
- the workbench answers an ask step in a form generated from its answer schema, with a question per field, and an ask step's output_schema can be built from earlier results
- graph plan draft, MCP graph_plan_draft and the workbench draft new plans with compose_plan, which asks about guessed values in one form and can ask for tools it wasn't shown; the step-by-step drafting overlay and its draft_* progress events are gone
- built-in tool_drafter and agent_drafter write tools and agents with you, test-running a tool before it is saved, and drafting one in the workbench is a mode with the draft in the pane
- graph wb opens the workbench at a front desk, graph wb plan|agent|tool go straight to a mode, orchestrator is renamed plan_drafter, and tool env values take ${VAR:-default}
- plan validate and plan run check literal step inputs against the called tool's input schema
- infer, decide and reshape are plain built-in tools instead of the always-on llm and data packs (#149)
- installed packs load per config layer and serve their tools, plans and agents under <pack>__<kind>__<name> names (#152)
- pack include/exclude narrow a pack's public interface, so only the pack's own plans and agents can call excluded items (#153)
- graph pack add, list, update and remove install and manage packs from git as plain copies per config layer (#156)
- packs declare graph, OS, binary, env and MCP server requirements that install, load, config check and runs enforce (#160)
- graph pack new, check, run, test and release let authors scaffold, validate, try, migration-test and semver-tag a pack (#161)
- graph pack update --major crosses majors one at a time, running each major's migration as a dry run, then applying it once confirmed (#162)
- CLI commands use singular nouns (graph tool, agent, thread, shape), with plural spellings kept as hidden aliases (#165)
- packs live in subdirectories of a repo with per-pack tags, and bare names install from tylerdavis/graph-packs (#166)
- graph pack add, update and list refuse a release tag that moved since it was installed (#168)
- graph pack check requires pack.toml to declare the systems a pack runs on (#170)

### Changed

- share one task-agent loop between the agent step and named agents; add stop tools, call sites and allowlisted registries
- pack remove restores a set-aside copy through one type, and refusals without problems use one helper (#159)

### Documentation

- rebuild the docs around agents, plans and tools, with reference pages generated from the code and fuller --help text (#154)
- the workbench screenshots show the github pack's tool names, and a stale screenshot fails the tests (#169)
- examples call the github pack's tools as github__tool__<name>, and this repo's checks run on the installed pack (#171)
- a Packs section explains installing, updating and writing packs, with a pack.toml reference generated from its schema (#172)

### Fixed

- builtin__infer runs as a non-interactive plan step, reporting empty data instead of asking for it
- gh_pr_ticket no longer reads prose such as "every 2" as a ticket, matching uppercase keys in titles and bodies and skipping standard names like UTF-8
- graph pack commands have help text for every argument and pages in the generated CLI reference (#158)

## Config v4 — 2026-10-14

### Breaking

- draft outlines from a coarse outliner role and a sectioned catalog; MCP servers take a description (config version 4) — config version 4 (graph config migrate)

## Plan v2 — 2026-10-14

### Breaking

- decide gates on exit, route and filter steps, and route steps with named cases chosen by a decision model; version 1 decide steps migrate to route — plan version 2 (graph plan migrate)

## Tool v2 — 2026-10-14

### Breaking

- decision tools and builtin__decide, answered by a decision model — tool version 2 (graph tools migrate)

## Agent v1 — 2026-10-14

- Introduced with Dive v0.16.0.

## Store v2 — 2026-10-14

### Breaking

- opening a data directory written before agents converts its threads to entry logs in place, and older graph versions then refuse it — store version 2

## Pack v1 — 2026-10-14

### Breaking

- pack.toml and packs.lock are versioned file formats, and config accepts [packs.<name>] entries (#150) — pack version 1 and lock version 1 are new file formats

## Lock v1 — 2026-10-14

- Introduced with Dive v0.16.0.

## Dive v0.15.0 — 2026-09-24

### Added

- edit steps and plan metadata in a form from the workbench plan tab (#128)

## Config v3 — 2026-09-24

### Breaking

- OTLP telemetry export for run traces and logs via [telemetry] (config version 3) (#129) — config version 3 (graph config migrate)

## Dive v0.14.0 — 2026-09-23

## Config v2 — 2026-09-23

### Breaking

- every model is a role: [models.<role>] replaces the fixed role slots and [models.named] (#125) — config version 2 (graph config migrate)

## Dive v0.13.0 — 2026-09-02

### Added

- version the config, plan, tool, and store file formats independently of the binary (#121)

### Fixed

- keep the tool rules out of `[prompts].workbench` so an override can't break the agent (#120)

## Config v1 — 2026-09-02

- Introduced with Dive v0.13.0.

## Plan v1 — 2026-09-02

- Introduced with Dive v0.13.0.

## Tool v1 — 2026-09-02

- Introduced with Dive v0.13.0.

## Store v1 — 2026-09-02

- Introduced with Dive v0.13.0.

## Dive v0.12.0 — 2026-08-12

### Added

- show each step's output contract in the workbench detail pane
- token usage and cost tracking, prompt caching, and a turn-key agent step (#115)

### Changed

- split graph_review_9000 into a local review core and a GitHub wrapper (#109)

### Documentation

- restore themed prose in the v0.11.0 changelog summary
- add use-graph, the general entry-point skill (#107)

### Fixed

- explain missing env vars at first use instead of dying silently

## Dive v0.11.0 — 2026-08-04

### Added

- add gh_pr_review_threads and gh_pr_thread_sync to the github pack
- track graph_review_9000 findings as living review threads

### Documentation

- publish a release changelog page, generated in lockstep with releases
- correct the escaping-mechanism comment in cliff-docs.toml
- infer a per-release summary and migration prompt via a graph plan
- keep inferred summaries MDX-safe
- drop the changelog page description and intro line
- replace the changelog shell scripts with graph plans
- place the migration prompt between summary and commit lists
- restyle changelog summaries as themed prose
- reference changelog snippets instead of embedding them

### Fixed

- release script keeps the docs release_version in step

## Dive v0.10.0 — 2026-08-04

### Added

- add --json to graph tools list
- add --json to graph tools show
- serve graph's plans and authoring commands over MCP
- stream progress and honor cancellation
- ask steps — put a question to the user from inside a plan
- filter steps — partition a list with a per-item gate
- per-file change objects on git_changed_files

### Changed

- return outcomes from the plan commands instead of printing
- convert the remaining commands and make --json uniform

### Documentation

- add a plan authoring skill for the CLI commands
- point the authoring loop and quickstart at the plan authoring skill
- make the authoring skill draft-first
- frame the skill around the whole plan lifecycle
- document graph as an MCP server
- close out the MCP roadmap and point the skill at the server
- explain why the draft arm cannot use and_then

### Fixed

- write every plan field in snake_case, at every depth
- remove revision-by-redraft from both authoring surfaces
- don't load a project the user never pointed at
- honour the documented ask retry budget
- serve the control steps from the tool catalog
- keep MCP writes inside the server's own config layer

## Dive v0.9.0 — 2026-07-30

### Breaking

- draft plans one validated step at a time, always — a config carrying `[planner]` now fails to load;

### Added

- model fallbacks for provider outage failover (#67)
- add the agent control step
- add gh_release and git_log to the github pack
- add slack pack with slack_post_message
- manage plans from the command line

### Changed

- lift plan authoring rules into graph-core

### Documentation

- rework intro/quickstart, add project-setup skill, tidy release asset names (#65)
- escape curly braces in template-language frontmatter (#66)
- reorganize the docs — plans-first IA, one owner per fact
- coherence pass after the reorg — catalog table parity, models links
- restructure the workbench page; split the exit-gates opener
- generate workbench screenshots from executed sessions
- inline Frame embeds — Mintlify snippets don't interpolate props into JSX attributes
- place workbench screenshots across the reorganized pages
- rewrite quickstart manual setup to mirror the fast path
- replace the introduction's plan-run snippet with a workbench hero shot
- document the agent control step
- document the plan authoring commands
- ship an app manifest and a one-click create-app button

### Fixed

- show never-run steps as skipped after a fired exit gate
- add fallbacks field to the shots harness ModelChoice
- close the agent step's validation, path, and boundary gaps
- carry exit codes back to main instead of exiting in place
- keep plan list and validate off stdout
- stop re-rendering caller-supplied reshape shapes

## Dive v0.8.1 — 2026-07-17

### Added

- show plan metadata, input schema, and finish on the root node (#61)

### Fixed

- wheel over steps/tool list moves selection (#60)
- gh_pr_ticket default pattern requires a separator (#62)
- wheel scrolls the steps/tool list view (#63)
- hide list highlight when selection scrolls out of view (#64)

## Dive v0.8.0 — 2026-07-17

### Added

- write the built-in system prompts into the config init starter (#44)
- steer check plans to explicit exits and list inference to map (#45)
- named models selectable from prompt tools and builtin__infer (#46)
- marker-keyed PR comments, ticket extraction, and file/grep at a ref (#47)
- catalog-aware tool resolution before any step runs (#49)
- incremental draft strategy — outline, then one validated step per inference (#50)
- edit input_schema, requires_servers, and silent finish via update_metadata (#51)
- mouse support — click to focus, switch tabs, select rows, wheel-scroll (#55)
- add data pack with builtin__reshape for shape projection (#58)
- optional per-gate model override on exit/decide infer (#59)

### Documentation

- add graph-github-actions-setup skill for coding agents (#42)

### Fixed

- render sub-text and borders with the terminal's dim modifier (#43)
- PR reviewer no longer emits absence false positives on truncated diffs (#40)
- paste literally, edit invalid drafts, repair bad drafts, fence agent-only tools (#48)
- separate outline and drafting phases in workbench trace (#52)
- show span start time on the left and duration on the right; surface outline call duration (#53)
- order trace chronologically so draft_plan brackets its phases; fix outline duration origin (#54)
- carry failing tool error into aborted run result (#56)
- default output_schema type + reset workbench iteration budget on progress (#57)

## Dive v0.7.0 — 2026-07-15

### Breaking

- replace LadybugDB with file-based storage (#24)

### Added

- plan workbench — dual-pane TUI for drafting and test-running plans (#25)
- workbench debug logging to <data_dir>/workbench.log (#28)
- workbench step view shows body sub-steps and the finish stage (#30)
- workbench read_file/grep/glob tools for researching the project (#29)
- step ids are any unique identifier, not just E-numbers (#31)
- workbench tools for precise plan edits: update_metadata, add_step, update_step, delete_step (#33)
- [prompts] config overrides for the chat prompt and workbench addendum (#37)
- project-first config — config init and default search paths target ./.graph (#38)
- draft safety, control-step guidance, turn-failure recovery (#36)

### Documentation

- cookbook covers a custom bot identity for the CI reviewer (#23)
- add @emichy to special thanks (#26)
- require worktrees for all coding work in CLAUDE.md (#39)

### Fixed

- scrolling reaches wrapped content; PgUp/PgDn is the one scroll binding (#27)
- draft saves can no longer overwrite a different plan's file (#32)
- section-scoped bare keys are not roots in plan validation (#34)
- a broken plan file no longer takes down the whole catalog (#35)

## Dive v0.6.0 — 2026-07-11

### Added

- decide steps fork plan execution into then/else branches (#15)
- decide gates read if/then/else — the logical gate keyword is now if (#17)
- map and reduce steps iterate a body over a list (#18)
- pr_review anchors findings as inline diff comments (#20)

## Dive v0.5.0 — 2026-07-10

### Added

- grouped listing for graph tools list, tighter layout (#14)

## Dive v0.4.1 — 2026-07-10

### Added

- group graph mcp tools output by server (#11)

### Documentation

- rewrite README, add MIT license (#12)

## Dive v0.4.0 — 2026-07-10

### Added

- publish a container image with each release (#4)
- vendor lbug fts/vector extensions into the binary (#9)
- builtin__ namespace for bundled tool packs, Built-ins docs page (#10)

### Documentation

- CI cookbook — the dogfooded plans and workflow, annotated (#7)
- cookbook as a section — pages by solution category (#8)

## Dive v0.3.0 — 2026-07-10

### Added

- bundled tool packs and GitHub Actions failure annotations (#2)

### Fixed

- portable version bump in release.sh; align workspace version with v0.2.0

## Dive v0.2.0 — 2026-07-10

### Added

- exit gates — end a plan early with success or error state
- plan composability — plans call plans

### Documentation

- bring CLAUDE.md current — composability, exit gates, storage, build story, conventions

## Dive v0.1.0 — 2026-07-10

### Added

- ladybug spike (validated) + layered config crate
- clap command tree, tracing, working config show/init/path
- provider trait, Anthropic + OpenAI-compat providers, structured output with repair, role router
- rmcp manager — stdio + streamable-http transports, lazy connect, tool discovery with namespacing and overrides, ToolRegistry impl
- ReAct loop + ask/chat/tools commands
- thread persistence + observed-shape cache (phase 3)
- unify thread continuation under --thread
- strict typed template engine for the {{Ex.path}} dialect
- plan pipeline — planner/validation/execution/solver with bus-driven replanning
- YAML plan docs, plans-as-tools, plan_and_execute (phase 4 complete)
- JSON input documents for plan run and tools test
- nested tool display, pipeline progress, streamed solver
- optional solver — plans can render structured output or run silently
- backend abstraction — dyn Store everywhere, memory backend
- user-defined tools — exec, cypher, and prompt kinds
- schema defaults for plan/tool inputs; fmt fixes
- codify release process — semver bump, git-cliff changelog, tag-driven binaries
- run traces — tools_used in ask envelope, GRAPH_EVENTS=jsonl event stream

### Documentation

- Mintlify documentation site (25 pages) + CLAUDE.md
- point repository URLs at the real remote
- fix clone directory in installation
- touch content to trigger first build
- remove build-trigger scratch line
- plans-first framing of core concepts
- quickstart — freeze-into-a-plan step
- plan-first nav order and README framing; drop unverified heading anchors

### Fixed

- shut down servers before runtime teardown; silence child stderr
- read the shape cache at each planning attempt
- steps_executed excludes the input root
- replace RUSTFLAGS with per-target build.rs link directives
