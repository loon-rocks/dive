//! Command-line interface definition.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "graph",
    version,
    long_version = crate::commands::version_cmd::LONG_VERSION.as_str(),
    about = "A command-line agent with a plan-based execution engine"
)]
pub struct Cli {
    /// Increase log verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Ask a one-shot question (runs one agent turn)
    #[command(
        after_long_help = "Runs one turn of the default `chat` agent, or the agent named with `--agent`, and streams its answer to stdout. Piped stdin is appended to the message as input context.\n\nPut `--thread` after the message, since it takes an optional value. On an existing thread, `--agent` must name the thread's owner.\n\nWith `--json`, prints `{content, tool_calls_made, tools_used, usage, thread_id, agent}` instead of streaming.\n\nExamples:\n  graph ask \"Which issues are blocking the release?\"\n  git diff | graph ask \"Review this diff\"\n  graph ask --agent release_notes \"What shipped this week?\"\n  graph ask \"And the week before?\" --thread"
    )]
    Ask {
        /// The message; reads stdin when omitted and piped
        message: Option<String>,
        /// Run a named agent instead of the default `chat` agent
        #[arg(long)]
        agent: Option<String>,
        /// Continue a thread: `--thread <id>` for a specific one, bare
        /// `--thread` for the most recent. Omit to start a new thread.
        #[arg(long)]
        thread: Option<Option<String>>,
        /// Emit a JSON envelope instead of streaming text
        #[arg(long)]
        json: bool,
        /// Print the final answer only, without streaming
        #[arg(long)]
        no_stream: bool,
    },
    /// Interactive chat (REPL)
    #[command(
        after_long_help = "Starts a conversation with the default `chat` agent or the named one. A resumed thread continues with its active agent; naming an agent other than the thread's owner is refused.\n\nSlash commands: `/agent` lists agents and marks the active one, `/agent <name>` switches to another, `/thread` and `/state` print the current thread and its state, and `/quit` exits.\n\nExamples:\n  graph chat\n  graph chat release_notes\n  graph chat --thread"
    )]
    Chat {
        /// The agent to talk to; the default `chat` agent when omitted
        agent: Option<String>,
        /// Continue a thread: `--thread <id>` for a specific one, bare
        /// `--thread` for the most recent. Omit to start a new thread.
        #[arg(long)]
        thread: Option<Option<String>>,
    },
    /// Manage and run plan documents
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Inspect the tool catalog (MCP, user, plan, and built-in tools)
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
    /// Inspect, validate, and migrate agent definitions
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },
    /// Manage conversation threads
    Threads {
        #[command(subcommand)]
        command: ThreadsCommand,
    },
    /// Manage MCP servers
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Inspect the observed-shape cache
    Shapes {
        #[command(subcommand)]
        command: ShapesCommand,
    },
    /// Show or initialize configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Interactive workbench: a dual-pane TUI for building plans, agents and tools
    #[command(visible_alias = "wb")]
    #[command(
        after_long_help = "Opens the full-screen workbench. With no subcommand it starts at the front desk, which asks what you want to build. Needs an interactive terminal on stdin and stdout.\n\nThe workbench logs to `<data_dir>/workbench.log` instead of stderr. Set `[workbench].log_path` or `GRAPH_WORKBENCH_LOG` to change the path.\n\nExamples:\n  graph wb"
    )]
    Workbench {
        #[command(subcommand)]
        command: Option<WorkbenchCommand>,
    },
    /// Print the binary version and the file formats it reads and writes
    #[command(
        after_long_help = "Prints the binary version and the version each kind of file is written at. `--json` also reports, under `fileVersions`, the oldest version of each the binary still reads. `graph --version` prints the same lines.\n\nExamples:\n  graph version\n  graph version --json"
    )]
    Version {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum WorkbenchCommand {
    /// Open the workbench in plan mode: draft a new plan, or open an existing
    /// one to inspect, edit and run — fully, or gated with per-tool-call confirmation
    #[command(after_long_help = "Examples:\n  graph wb plan\n  graph wb plan weekly_changes")]
    Plan {
        /// A plan identifier or YAML file path to open; omit for a blank draft
        name_or_path: Option<String>,
    },
    /// Open the workbench in agent mode: draft a new agent, or edit an existing one
    #[command(after_long_help = "Examples:\n  graph wb agent\n  graph wb agent release_notes")]
    Agent {
        /// An agent name to open for editing; omit to draft a new one
        name: Option<String>,
    },
    /// Open the workbench in tool mode: draft a new user tool, or edit an existing one
    #[command(after_long_help = "Examples:\n  graph wb tool\n  graph wb tool summarize")]
    Tool {
        /// A user tool name to open for editing; omit to draft a new one
        name: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PlanCommand {
    /// List available plan documents
    #[command(
        after_long_help = "Lists the runnable catalog. `--json` also reports plans that failed to load under `skipped`, and plans hidden by unconfigured `requires_servers` under `hidden`.\n\nExamples:\n  graph plan list\n  graph plan list --json"
    )]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Show a plan document
    #[command(
        after_long_help = "Prints the plan's YAML to stdout and its source, `built-in` or the file path, to stderr. A plan file whose identifier matches a built-in plan replaces it. Opens plans that are invalid or hidden by `requires_servers`.\n\nExamples:\n  graph plan show weekly_changes\n  graph plan show .graph/plans/weekly_changes.yaml --json"
    )]
    Show {
        #[arg(help = "The plan's identifier or YAML file path")]
        name: String,
        /// Emit the document as JSON instead of YAML
        #[arg(long)]
        json: bool,
    },
    /// Validate a plan document by name or file path
    #[command(
        after_long_help = "Checks structure (templates, references, step ids) and resolves every step's tool against the catalog: packs, user tools, plans, agents, and configured MCP servers. MCP tool names are checked at the server level only.\n\nProblems exit 1; success prints nothing to stdout. A plan whose `requires_servers` names an unconfigured server validates with a note.\n\nExamples:\n  graph plan validate weekly_changes\n  graph plan validate .graph/plans/weekly_changes.yaml"
    )]
    Validate {
        #[arg(help = "The plan's identifier or YAML file path")]
        name_or_path: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Rewrite a plan file to the current plan format
    #[command(
        after_long_help = "Rewrites one plan file to the current plan file version. A file already at the current version is left untouched.\n\nExamples:\n  graph plan migrate .graph/plans/weekly_changes.yaml"
    )]
    Migrate {
        #[arg(help = "The plan's identifier or YAML file path")]
        name_or_path: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Run a plan directly (bypasses the agent loop)
    #[command(
        after_long_help = "Resolves every step's tool first and refuses to start if any is missing. A solver plan streams its answer to stdout, an output plan prints its JSON, and a silent plan prints nothing.\n\nMissing or invalid inputs exit 3 with the input schema on stderr. An exit step that asserts an error exits 4.\n\nExamples:\n  graph plan run weekly_changes\n  graph plan run weekly_changes '{\"base\":\"v0.15.0\"}'\n  graph plan run weekly_changes @inputs.json --input base=v0.14.0"
    )]
    Run {
        #[arg(help = "The plan's identifier, as `graph plan list` prints it")]
        name: String,
        /// Inputs as a JSON object: inline (`'{"a":1}'`), `@file.json`, or `-` for stdin
        #[arg(value_name = "JSON|@FILE|-")]
        input: Option<String>,
        /// Override individual input keys (applied on top of the JSON document)
        #[arg(long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Scaffold an empty plan file to build up with `set` and `step add`
    #[command(
        after_long_help = "Writes a plan with no steps, which won't validate until you add one. An edit is refused only when it adds a validation problem, so you can build the plan up one step at a time.\n\nExamples:\n  graph plan new weekly_changes --description \"Summarize what changed in this repo this week\""
    )]
    New {
        /// Tool-name-safe identifier; also the file name
        identifier: String,
        /// Display name (defaults to the identifier)
        #[arg(long)]
        name: Option<String>,
        /// What the plan does — the agent's routing signal
        #[arg(long)]
        description: Option<String>,
        /// Write here instead of <plans dir>/<identifier>.yaml
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Draft a plan from a goal using the planner model (costs inference)
    #[command(
        after_long_help = "Runs the built-in `compose_plan` plan over your tool catalog: it finds the tools the goal needs, writes the whole plan in one call, and fixes steps that fail validation. When it had to guess a value that decides what the plan returns, it asks you in the terminal first.\n\nProblems it couldn't fix are listed under `problems`. When no tool in the catalog can do part of the goal, nothing is written and the result lists what is `missing`.\n\nDrafting replaces every step. To correct a plan whose steps you want to keep, use `graph plan set` and the `graph plan step` commands.\n\nExamples:\n  graph plan draft \"Summarize the commits merged since the last release\"\n  graph plan draft \"Summarize the commits merged since the last release\" --from weekly_changes\n  graph plan draft \"List open pull requests with failing checks\" --stdout"
    )]
    Draft {
        /// What the plan should do, as a self-contained instruction
        goal: String,
        /// Draft into this plan's identity (identifier, name, description,
        /// input schema). Every step is replaced — use the editing commands
        /// to correct a plan whose steps you want to keep.
        #[arg(long, value_name = "NAME|PATH")]
        from: Option<String>,
        /// Write here instead of <plans dir>/<identifier>.yaml
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        /// Print the drafted YAML to stdout instead of writing a file
        #[arg(long)]
        stdout: bool,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Set one plan attribute
    #[command(
        after_long_help = "Sets one attribute, named as it appears in the plan file. `solver` and `output` are the two finish modes, so setting one clears the other. Changing `identifier` writes a new file and leaves the old one in place.\n\nAn edit is refused if it adds a validation problem, and a refused edit exits 1 and leaves the file untouched. Problems that were already there never block an edit; they are reported as `preExistingProblems`.\n\nEvery edit rewrites the whole file, so YAML comments are dropped and flow mappings become block mappings.\n\nExamples:\n  graph plan set weekly_changes description \"Summarize the commits merged since a ref\"\n  graph plan set weekly_changes exemplars \"What changed this week?\" \"What shipped since v0.15.0?\"\n  graph plan set weekly_changes input_schema @schema.json"
    )]
    Set {
        #[arg(
            value_name = "NAME|PATH",
            help = "The plan's identifier or YAML file path"
        )]
        target: String,
        #[arg(help = "The attribute to set")]
        attribute: PlanAttribute,
        /// One value; `exemplars` and `requires_servers` accept several.
        /// `input_schema`, `solver`, and `output` take JSON, @file, or -
        #[arg(required = true, value_name = "VALUE")]
        value: Vec<String>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Clear one optional plan attribute
    #[command(
        after_long_help = "Clears one optional attribute. Clearing both `solver` and `output` leaves a silent plan. `identifier`, `name` and `description` are required and can only be changed with `graph plan set`.\n\nExamples:\n  graph plan unset weekly_changes exemplars"
    )]
    Unset {
        #[arg(
            value_name = "NAME|PATH",
            help = "The plan's identifier or YAML file path"
        )]
        target: String,
        #[arg(help = "The attribute to clear")]
        attribute: PlanAttribute,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Add, edit, or remove a plan's steps
    Step {
        #[command(subcommand)]
        command: StepCommand,
    },
}

/// Plan-level attributes addressable by `graph plan set` / `unset`. Named
/// exactly as they appear in the plan YAML, so what an agent reads in a plan
/// file is what it writes on the command line.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PlanAttribute {
    Name,
    Description,
    Identifier,
    Exemplars,
    #[value(name = "requires_servers", alias = "requires-servers")]
    RequiresServers,
    #[value(name = "input_schema", alias = "input-schema")]
    InputSchema,
    Solver,
    Output,
}

impl PlanAttribute {
    /// The attribute's name as spelled on the command line and in the YAML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Description => "description",
            Self::Identifier => "identifier",
            Self::Exemplars => "exemplars",
            Self::RequiresServers => "requires_servers",
            Self::InputSchema => "input_schema",
            Self::Solver => "solver",
            Self::Output => "output",
        }
    }
}

/// Step attributes addressable by `graph plan step update` / `unset`.
/// A step's `id` is deliberately absent: renaming rewrites downstream
/// `{{id.*}}` references, so it gets its own verb (`step rename`) rather
/// than looking like a plain field write.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StepAttribute {
    #[value(name = "tool", alias = "tool_name")]
    Tool,
    Input,
    Reasoning,
}

impl StepAttribute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Input => "input",
            Self::Reasoning => "reasoning",
        }
    }
}

impl StepCommand {
    /// Whether `--json` was asked for. Presentation is the dispatch layer's
    /// business, so the command functions themselves never read this — they
    /// return an `Outcome` and let the caller decide how to render it.
    pub fn json(&self) -> bool {
        match self {
            Self::Add { json, .. }
            | Self::Update { json, .. }
            | Self::Rename { json, .. }
            | Self::Unset { json, .. }
            | Self::Rm { json, .. } => *json,
        }
    }
}

#[derive(Subcommand)]
pub enum StepCommand {
    /// Append a step, or anchor it before/after an existing one
    #[command(
        after_long_help = "Adds one step and validates the plan. The edit is refused if it adds a validation problem.\n\nExamples:\n  graph plan step add weekly_changes E1 builtin__git_log '{\"base\":\"{{input.base}}\",\"head\":\"HEAD\"}'\n  graph plan step add weekly_changes G1 exit '{\"when\":{\"value\":\"{{E1.count}}\",\"op\":\"eq\",\"to\":0},\"status\":\"success\",\"message\":\"Nothing shipped\"}' --after E1"
    )]
    Add {
        #[arg(
            value_name = "NAME|PATH",
            help = "The plan's identifier or YAML file path"
        )]
        target: String,
        /// Step id — how later steps reference it as {{<id>.field}}
        id: String,
        /// Tool name, or a control step: exit, agent, ask, route, filter, map, reduce
        #[arg(value_name = "TOOL")]
        tool: String,
        /// The step's input object: inline JSON, @file.json, or - for stdin
        #[arg(value_name = "JSON|@FILE|-")]
        input: String,
        /// Why this step exists (carried into the plan for readers)
        #[arg(long)]
        reasoning: Option<String>,
        /// Insert before this step id instead of appending
        #[arg(long, value_name = "ID", conflicts_with = "after")]
        before: Option<String>,
        /// Insert after this step id instead of appending
        #[arg(long, value_name = "ID")]
        after: Option<String>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Set one attribute of a step
    #[command(
        after_long_help = "Sets one attribute of a step. `tool` takes a tool name or a control step (`exit`, `agent`, `ask`, `route`, `filter`, `map`, `reduce`), `input` takes the step's whole input object, and `reasoning` takes a string.\n\nExamples:\n  graph plan step update weekly_changes E1 reasoning \"Commit subjects are enough for a summary\"\n  graph plan step update weekly_changes E1 input @e1.json"
    )]
    Update {
        #[arg(
            value_name = "NAME|PATH",
            help = "The plan's identifier or YAML file path"
        )]
        target: String,
        #[arg(help = "The step's id")]
        id: String,
        #[arg(help = "The attribute to set")]
        attribute: StepAttribute,
        /// `input` takes JSON, @file, or -; the others take a string
        #[arg(value_name = "VALUE")]
        value: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Rename a step, rewriting every downstream {{id.*}} reference
    #[command(
        after_long_help = "Renames a step and rewrites every reference to it in later steps, the `output` map and the `solver` templates.\n\nExamples:\n  graph plan step rename weekly_changes E1 commits"
    )]
    Rename {
        #[arg(
            value_name = "NAME|PATH",
            help = "The plan's identifier or YAML file path"
        )]
        target: String,
        #[arg(help = "The step's id")]
        id: String,
        #[arg(help = "The new step id")]
        new_id: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Clear one optional attribute of a step
    #[command(
        after_long_help = "Clears one optional attribute of a step. A step's `tool` and `input` are required and can't be cleared.\n\nExamples:\n  graph plan step unset weekly_changes E1 reasoning"
    )]
    Unset {
        #[arg(
            value_name = "NAME|PATH",
            help = "The plan's identifier or YAML file path"
        )]
        target: String,
        #[arg(help = "The step's id")]
        id: String,
        #[arg(help = "The attribute to clear")]
        attribute: StepAttribute,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Remove a step
    #[command(
        after_long_help = "Removes a step. The edit is refused while a later step still references it.\n\nExamples:\n  graph plan step rm weekly_changes E2"
    )]
    Rm {
        #[arg(
            value_name = "NAME|PATH",
            help = "The plan's identifier or YAML file path"
        )]
        target: String,
        #[arg(help = "The step's id")]
        id: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ToolsCommand {
    /// List every tool visible to the agent and planner
    #[command(
        after_long_help = "Groups the catalog by source with one-line descriptions. `--json` prints the flat catalog instead: each tool's namespaced name, `source`, full `description`, `readOnly` and `hasOutputSchema`.\n\nExamples:\n  graph tools list\n  graph tools list --json"
    )]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Show one tool's description and schemas
    #[command(
        after_long_help = "For a `builtin__` or `user__` tool, also names its source on stderr: the tool file's path, or `built-in (<pack> pack)`. `--json` prints the whole definition, including `inputSchema`, with absent keys as `null`.\n\nExamples:\n  graph tools show builtin__git_log\n  graph tools show linear__list_issues --json"
    )]
    Show {
        #[arg(help = "The tool's namespaced name, as `graph tools list` prints it")]
        name: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Invoke a tool directly
    #[command(
        after_long_help = "Calls the tool and prints its result. With `--json`, prints `{tool, isError, result}`; a tool that reports an error still exits 0, with `isError` set.\n\nExamples:\n  graph tools test builtin__git_log '{\"base\":\"v0.15.0\",\"head\":\"HEAD\"}'\n  graph tools test builtin__git_log --input base=v0.15.0 --input head=HEAD --json"
    )]
    Test {
        #[arg(help = "The tool's namespaced name, as `graph tools list` prints it")]
        name: String,
        /// Input as a JSON object: inline (`'{"a":1}'`), `@file.json`, or `-` for stdin
        #[arg(value_name = "JSON|@FILE|-")]
        input: Option<String>,
        /// Override individual input keys (applied on top of the JSON document)
        #[arg(long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Rewrite a user tool file to the current tool format
    #[command(
        after_long_help = "Rewrites one user tool file to the current tool file version. It takes a path rather than a tool name and doesn't connect to MCP servers.\n\nExamples:\n  graph tools migrate .graph/tools/summarize.yaml"
    )]
    Migrate {
        #[arg(help = "Path to the user tool file")]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum AgentsCommand {
    /// List every agent: built-in, global (~/.config/graph/agents), and project (./.graph/agents)
    #[command(
        after_long_help = "Prints each agent's name, source and description. `--json` adds its model, subagents, handoffs, how it is called as a subagent (`subagentIo`), and any files that failed to load (`loadErrors`).\n\nExamples:\n  graph agents list\n  graph agents list --json"
    )]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Print an agent's effective definition, the file to copy when customizing it
    #[command(
        after_long_help = "Examples:\n  graph agents show chat\n  graph agents show chat > .graph/agents/chat.yaml"
    )]
    Show {
        #[arg(help = "The agent's name")]
        name: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Validate every agent, or one agent file against the others
    #[command(
        after_long_help = "Problems exit 1. `--json` lists them under `problems`.\n\nExamples:\n  graph agents validate\n  graph agents validate .graph/agents/release_notes.yaml"
    )]
    Validate {
        #[arg(help = "One agent file to check against the others; omit to check every agent")]
        path: Option<PathBuf>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Rewrite an agent file to the current agent format
    #[command(
        after_long_help = "Rewrites one agent file to the current agent file version.\n\nExamples:\n  graph agents migrate .graph/agents/release_notes.yaml"
    )]
    Migrate {
        #[arg(help = "Path to the agent file")]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ThreadsCommand {
    /// List threads
    #[command(after_long_help = "Examples:\n  graph threads list")]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Show a thread's messages
    #[command(
        after_long_help = "Examples:\n  graph threads show 4f1c9a2b7d3e\n  graph threads show 4f1c9a2b7d3e --state --json"
    )]
    Show {
        #[arg(help = "The thread id, as `graph threads list` prints it")]
        id: String,
        /// Include the full runtime state
        #[arg(long)]
        state: bool,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Delete a thread
    #[command(after_long_help = "Examples:\n  graph threads rm 4f1c9a2b7d3e")]
    Rm {
        #[arg(help = "The thread id, as `graph threads list` prints it")]
        id: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum McpCommand {
    /// List configured servers and their status
    #[command(after_long_help = "Examples:\n  graph mcp list")]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// List tools exposed by servers
    #[command(after_long_help = "Examples:\n  graph mcp tools\n  graph mcp tools linear --json")]
    Tools {
        #[arg(help = "Only this server; omit for every configured server")]
        server: Option<String>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Connect to a server and verify initialize + tools/list
    #[command(after_long_help = "Examples:\n  graph mcp test linear")]
    Test {
        #[arg(help = "The server name, as configured under `[mcp.<name>]`")]
        server: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Serve graph's plans and authoring commands to an agent over MCP (stdio)
    #[command(after_long_help = "Examples:\n  graph mcp serve --dir ~/code/my-project")]
    Serve {
        /// Project directory to serve from — the one holding `.graph/`.
        /// MCP clients launch servers with an arbitrary working directory,
        /// so without this the plan catalog resolves against wherever the
        /// client happened to be.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
    /// Pre-warm the observed-shape cache by invoking read-only tools
    #[command(after_long_help = "Examples:\n  graph mcp probe\n  graph mcp probe linear")]
    Probe {
        #[arg(help = "Only this server; omit for every configured server")]
        server: Option<String>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ShapesCommand {
    /// List cached tool shapes
    #[command(after_long_help = "Examples:\n  graph shapes list")]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Show one tool's cached schema and example
    #[command(after_long_help = "Examples:\n  graph shapes show linear__list_issues")]
    Show {
        #[arg(help = "The tool's namespaced name")]
        tool: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print the merged effective configuration
    #[command(after_long_help = "Examples:\n  graph config show\n  graph config show --json")]
    Show {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Write a starter config file
    #[command(
        after_long_help = "Writes a commented starter config to `./.graph/config.toml`, or to `~/.config/graph/config.toml` with `--global`. Refuses to overwrite an existing file unless `--force` is set.\n\nExamples:\n  graph config init\n  graph config init --global"
    )]
    Init {
        /// Write to the global location (~/.config/graph/) instead of the project (./.graph/)
        #[arg(long)]
        global: bool,
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
        #[allow(dead_code)]
        #[arg(long, hide = true)]
        path: Option<PathBuf>,
    },
    /// Print the config file locations and which exist
    #[command(after_long_help = "Examples:\n  graph config path")]
    Path {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Report each config file's format and whether the merged config loads
    #[command(
        after_long_help = "Reports each config file's version as `current`, `migrate`, `too new` or `too old`, then loads the merged config and exits 1 with the problems if it doesn't load.\n\nExamples:\n  graph config check"
    )]
    Check {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Rewrite a config file to the current config format, comments intact
    #[command(
        after_long_help = "Rewrites the config file to the current config file version, keeping comments and layout. A file already at the current version is left untouched.\n\nExamples:\n  graph config migrate\n  graph config migrate --global"
    )]
    Migrate {
        /// Migrate the global file (~/.config/graph/) instead of the project one (./.graph/)
        #[arg(long)]
        global: bool,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_well_formed() {
        // clap's own consistency check: duplicate arg ids, conflicting short
        // flags, `required` on a positional after an optional one. All of it
        // otherwise panics at runtime, on the user's terminal, and only for
        // the one subcommand that happens to be broken.
        Cli::command().debug_assert();
    }
}
