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
    #[command(alias = "plans")]
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Inspect the tool catalog (MCP, user, plan, and built-in tools)
    #[command(alias = "tools")]
    Tool {
        #[command(subcommand)]
        command: ToolCommand,
    },
    /// Inspect, validate, and migrate agent definitions
    #[command(alias = "agents")]
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Manage conversation threads
    #[command(alias = "threads")]
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// Manage MCP servers
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Inspect the observed-shape cache
    #[command(alias = "shapes")]
    Shape {
        #[command(subcommand)]
        command: ShapeCommand,
    },
    /// Show or initialize configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    #[command(
        about = "Install, list, update and remove packs of tools, plans and agents from git",
        alias = "packs"
    )]
    Pack {
        #[command(subcommand)]
        command: PackCommand,
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
    #[command(
        alias = "plans",
        after_long_help = "Examples:\n  graph wb plan\n  graph wb plan weekly_changes"
    )]
    Plan {
        /// A plan identifier or YAML file path to open; omit for a blank draft
        name_or_path: Option<String>,
    },
    /// Open the workbench in agent mode: draft a new agent, or edit an existing one
    #[command(
        alias = "agents",
        after_long_help = "Examples:\n  graph wb agent\n  graph wb agent release_notes"
    )]
    Agent {
        /// An agent name to open for editing; omit to draft a new one
        name: Option<String>,
    },
    /// Open the workbench in tool mode: draft a new user tool, or edit an existing one
    #[command(
        alias = "tools",
        after_long_help = "Examples:\n  graph wb tool\n  graph wb tool summarize"
    )]
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
        after_long_help = concat!("Resolves every step's tool first and refuses to start if any is missing. A solver plan streams its answer to stdout, an output plan prints its JSON, and a silent plan prints nothing.\n\nMissing or invalid inputs exit 3 with the input schema on stderr. An exit step that asserts an error exits 4.\n\nExamples:\n  graph plan run weekly_changes\n  graph plan run weekly_changes '{\"base\":\"", "v", env!("CARGO_PKG_VERSION"), "\"}'\n  graph plan run weekly_changes @inputs.json --input base=v0.14.0")
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
        after_long_help = concat!("Sets one attribute, named as it appears in the plan file. `solver` and `output` are the two finish modes, so setting one clears the other. Changing `identifier` writes a new file and leaves the old one in place.\n\nAn edit is refused if it adds a validation problem, and a refused edit exits 1 and leaves the file untouched. Problems that were already there never block an edit; they are reported as `preExistingProblems`.\n\nEvery edit rewrites the whole file, so YAML comments are dropped and flow mappings become block mappings.\n\nExamples:\n  graph plan set weekly_changes description \"Summarize the commits merged since a ref\"\n  graph plan set weekly_changes exemplars \"What changed this week?\" \"What shipped since ", "v", env!("CARGO_PKG_VERSION"), "?\"\n  graph plan set weekly_changes input_schema @schema.json")
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
        after_long_help = "Adds one step and validates the plan. The edit is refused if it adds a validation problem.\n\nExamples:\n  graph plan step add weekly_changes E1 github__tool__git_log '{\"base\":\"{{input.base}}\",\"head\":\"HEAD\"}'\n  graph plan step add weekly_changes G1 exit '{\"when\":{\"value\":\"{{E1.count}}\",\"op\":\"eq\",\"to\":0},\"status\":\"success\",\"message\":\"Nothing shipped\"}' --after E1"
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
pub enum ToolCommand {
    /// List every tool visible to the agent and planner
    #[command(
        after_long_help = "Groups the catalog by source with one-line descriptions. `--json` prints the flat catalog instead: each tool's namespaced name, `source`, full `description`, `readOnly` and `hasOutputSchema`.\n\nExamples:\n  graph tool list\n  graph tool list --json"
    )]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Show one tool's description and schemas
    #[command(
        after_long_help = "For a `builtin__`, `user__` or pack tool, also names its source on stderr: the tool file's path, `built-in`, or `pack <name> <tag> (<layer>)`. `--json` prints the whole definition, including `inputSchema`, with absent keys as `null`.\n\nExamples:\n  graph tool show github__tool__git_log\n  graph tool show linear__list_issues --json"
    )]
    Show {
        #[arg(help = "The tool's namespaced name, as `graph tool list` prints it")]
        name: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Invoke a tool directly
    #[command(
        after_long_help = concat!("Calls the tool and prints its result. With `--json`, prints `{tool, isError, result}`; a tool that reports an error still exits 0, with `isError` set.\n\nExamples:\n  graph tool test github__tool__git_log '{\"base\":\"", "v", env!("CARGO_PKG_VERSION"), "\",\"head\":\"HEAD\"}'\n  graph tool test github__tool__git_log --input base=", "v", env!("CARGO_PKG_VERSION"), " --input head=HEAD --json")
    )]
    Test {
        #[arg(help = "The tool's namespaced name, as `graph tool list` prints it")]
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
        after_long_help = "Rewrites one user tool file to the current tool file version. It takes a path rather than a tool name and doesn't connect to MCP servers.\n\nExamples:\n  graph tool migrate .graph/tools/summarize.yaml"
    )]
    Migrate {
        #[arg(help = "Path to the user tool file")]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// List every agent: built-in, global (~/.config/graph/agents), and project (./.graph/agents)
    #[command(
        after_long_help = "Prints each agent's name, source and description. `--json` adds its model, subagents, handoffs, how it is called as a subagent (`subagentIo`), and any files that failed to load (`loadErrors`).\n\nExamples:\n  graph agent list\n  graph agent list --json"
    )]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Print an agent's effective definition, the file to copy when customizing it
    #[command(
        after_long_help = "Examples:\n  graph agent show chat\n  graph agent show chat > .graph/agents/chat.yaml"
    )]
    Show {
        #[arg(help = "The agent's name")]
        name: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Validate every agent, or one agent file against the others
    #[command(
        after_long_help = "Problems exit 1. `--json` lists them under `problems`.\n\nExamples:\n  graph agent validate\n  graph agent validate .graph/agents/release_notes.yaml"
    )]
    Validate {
        #[arg(help = "One agent file to check against the others; omit to check every agent")]
        path: Option<PathBuf>,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Rewrite an agent file to the current agent format
    #[command(
        after_long_help = "Rewrites one agent file to the current agent file version.\n\nExamples:\n  graph agent migrate .graph/agents/release_notes.yaml"
    )]
    Migrate {
        #[arg(help = "Path to the agent file")]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ThreadCommand {
    /// List threads
    #[command(after_long_help = "Examples:\n  graph thread list")]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Show a thread's messages
    #[command(
        after_long_help = "Examples:\n  graph thread show 4f1c9a2b7d3e\n  graph thread show 4f1c9a2b7d3e --state --json"
    )]
    Show {
        #[arg(help = "The thread id, as `graph thread list` prints it")]
        id: String,
        /// Include the full runtime state
        #[arg(long)]
        state: bool,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Delete a thread
    #[command(after_long_help = "Examples:\n  graph thread rm 4f1c9a2b7d3e")]
    Rm {
        #[arg(help = "The thread id, as `graph thread list` prints it")]
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
pub enum ShapeCommand {
    /// List cached tool shapes
    #[command(after_long_help = "Examples:\n  graph shape list")]
    List {
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    /// Show one tool's cached schema and example
    #[command(after_long_help = "Examples:\n  graph shape show linear__list_issues")]
    Show {
        #[arg(help = "The tool's namespaced name")]
        tool: String,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
}

#[derive(clap::Args)]
pub struct LayerArgs {
    #[arg(
        long,
        conflicts_with = "dir",
        help = "Target the global layer (~/.config/graph/) instead of the project's ./.graph/"
    )]
    pub global: bool,
    #[arg(
        long,
        value_name = "PATH",
        help = "Target this graph directory, laid out like .graph/ (config.toml, packs/, packs.lock)"
    )]
    pub dir: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum PackCommand {
    #[command(
        about = "Install a pack from git as a plain copy in the project, global or given graph directory",
        after_long_help = "packs.lock records the commit each release tag pointed at when it was installed. A published release must never move: when the repository's copy of a recorded tag now points elsewhere, or a clone checks out a different commit than git ls-remote reported, the install is refused and nothing changes. Branch-tracking packs are exempt."
    )]
    Add {
        #[arg(
            value_name = "NAME|URL",
            help = "A bare name (the <name> directory of github.com/tylerdavis/graph-packs) or a git URL; a name already in [packs] installs that entry"
        )]
        source: String,
        #[arg(
            long,
            value_name = "DIR",
            help = "The pack's directory inside the repository, whose releases are the tags <DIR>/vX.Y.Z; default: the repository root (a bare name's own directory)"
        )]
        path: Option<String>,
        #[arg(
            long = "as",
            value_name = "NAME",
            help = "Install under this name instead of the manifest's"
        )]
        as_name: Option<String>,
        #[arg(
            long = "ref",
            value_name = "REF",
            help = "A major line (v2), an exact release (v2.1.0, pinned) or a branch; default: the newest release"
        )]
        reference: Option<String>,
        #[command(flatten)]
        layer: LayerArgs,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "List installed packs with their tags, newer releases, local edits, shadowing and moved release tags"
    )]
    List {
        #[command(flatten)]
        layer: LayerArgs,
        #[arg(
            long,
            help = "Skip the git ls-remote per pack and the columns it fills"
        )]
        offline: bool,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "Move packs to the newest release their ref allows, or with --major across majors, migrating your files at each",
        after_long_help = "A pack whose installed release tag now points at a different commit than packs.lock recorded is refused and left as installed: a published release must never move. The same holds when a clone checks out a different commit than git ls-remote reported. Branch-tracking packs are exempt, and a pinned pack is skipped without contacting the remote."
    )]
    Update {
        #[arg(
            value_name = "NAME",
            help = "Packs to update; every pack in the layer when omitted"
        )]
        names: Vec<String>,
        #[arg(long, help = "Overwrite a copy whose content differs from packs.lock")]
        force: bool,
        #[arg(
            long,
            help = "Cross to each newer major in turn: run its migration as a dry run, show the diffs, ask, apply, then move ref to that major"
        )]
        major: bool,
        #[arg(
            long,
            requires = "major",
            help = "Accept every migration and major without asking (required when there is no terminal)"
        )]
        yes: bool,
        #[command(flatten)]
        layer: LayerArgs,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "Delete a pack's copy, config entry and lock entry, and list files that still call it"
    )]
    Remove {
        #[arg(value_name = "NAME", help = "The installed pack to remove")]
        name: String,
        #[command(flatten)]
        layer: LayerArgs,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "Scaffold a pack in a directory of a packs repository: manifest, one tool, plan and agent, README, CHANGELOG and MIGRATING.md",
        after_long_help = "Writes the files from a scaffold built into graph, so it works offline, into <PATH>/<NAME>/, which is created when missing and refused when it holds anything. A packs repository holds one pack per directory, each released as `<NAME>/vX.Y.Z` tags. When the repository has no `.github/workflows/packs.yaml`, it also gets one that runs `graph pack test` over every pack, and a `.gitignore` for `.graph/` when it has none. The scaffold passes `graph pack check` and `graph pack test` as written.\n\nExamples:\n  graph pack new files\n  graph pack new files ../graph-packs"
    )]
    New {
        #[arg(
            value_name = "NAME",
            help = "The pack's name, written into pack.toml and used as its directory: lowercase letters, digits and underscores, at most 20 characters"
        )]
        name: String,
        #[arg(
            value_name = "PATH",
            default_value = ".",
            help = "The packs repository's root; the pack goes in <PATH>/<NAME>/"
        )]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "Load a pack the way an install would and report every problem: manifest, files, references, names, migrations and requirements",
        after_long_help = "Loads the pack under its manifest name and checks every plan and agent against the pack plus graph's built-ins, including each step's literal input against the called tool's input schema. pack.toml must list the systems the pack runs on in `requires.os` (`macos`, `linux`). Calls to MCP servers named in `requires.mcp` are checked against the server when one is configured under that name, and reported as unchecked otherwise. A directory without pack.toml is a packs repository: every immediate subdirectory holding one is checked. Exits 1 when there is any problem.\n\nExamples:\n  graph pack check\n  graph pack check files --json"
    )]
    Check {
        #[arg(
            value_name = "PATH",
            default_value = ".",
            help = "The pack's directory, holding pack.toml, or a packs repository whose subdirectories hold packs"
        )]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "Run one of a pack's items, private and migration ones included, under its manifest name",
        after_long_help = "Runs a tool, plan or agent of the pack at --path, the way the pack's own plans call it, so items outside the public interface and migration items run too. This is the only direct way to run them. The result is printed to stdout as JSON.\n\nExamples:\n  graph pack run tool__word_count --input file=README.md\n  graph pack run plan__migrate_v2 '{\"root\": \".graph\", \"apply\": false}' --path files"
    )]
    Run {
        #[arg(
            value_name = "ITEM",
            help = "The item as <tool|plan|agent>__<name>, its full catalog name, $pack__<kind>__<name>, or a bare name when only one item has it"
        )]
        item: String,
        #[arg(
            value_name = "JSON|@FILE|-",
            help = "Inputs as a JSON object: inline ('{\"a\":1}'), @file.json, or - for stdin"
        )]
        input: Option<String>,
        #[arg(
            long = "input",
            value_name = "KEY=VALUE",
            help = "Override individual input keys (applied on top of the JSON document)"
        )]
        inputs: Vec<String>,
        #[arg(
            long,
            value_name = "PATH",
            default_value = ".",
            help = "The pack's directory, holding pack.toml"
        )]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "Run check, then every migration over its sample files in tests/migrations/v<N>/ and validate the result",
        after_long_help = "For each `tests/migrations/v<N>/`, copies it to a temporary directory, runs `migrate_v<N>` and every later migration with `apply: true`, then validates the migrated plans, agents and tools against the pack as it is now: every pack item they call must be public, and every literal step input must match the called tool's input schema. Migrations that call `builtin__infer` use your configured model providers. Exits 1 on any problem, naming the file. A directory without pack.toml is a packs repository: every immediate subdirectory holding one is tested, so a repository's CI is `graph pack test` at its root.\n\nExamples:\n  graph pack test\n  graph pack test files --json"
    )]
    Test {
        #[arg(
            value_name = "PATH",
            default_value = ".",
            help = "The pack's directory, holding pack.toml, or a packs repository whose subdirectories hold packs"
        )]
        path: PathBuf,
        #[arg(long, help = "Print a JSON envelope instead of text")]
        json: bool,
    },
    #[command(
        about = "Compare the public interface with the previous release tag and create the next annotated tag, without pushing",
        after_long_help = "A pack's releases are the tags `<DIR>/vX.Y.Z`, where <DIR> is the pack's directory relative to the repository root (plain `vX.Y.Z` for a pack at the root). Refuses uncommitted changes inside the pack's directory, then compares every public item's name, input schema and output schema with the pack's newest release tag. A removed or renamed item, a new required input, or a removed or retyped input or output property needs a major; a new item, optional input or output property needs at least a minor. A major also needs `plans/migrations/migrate_v<N>.yaml` or a `## v<N-1> to v<N>` section in MIGRATING.md. Then it runs `graph pack test` and creates the annotated tag. The first release skips the comparison and is v1.0.0 for major, v0.1.0 for minor and v0.0.1 for patch. Push the tag yourself.\n\nExamples:\n  graph pack release minor files\n  graph pack release major ../graph-packs/files"
    )]
    Release {
        #[arg(value_name = "BUMP", help = "Which part of the version to raise")]
        bump: graph_runtime::packs::release::Bump,
        #[arg(
            value_name = "PATH",
            default_value = ".",
            help = "The pack's directory, anywhere inside its git repository"
        )]
        path: PathBuf,
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

    #[test]
    fn plural_command_names_resolve_to_their_singular() {
        let root = Cli::command();
        let workbench = root.find_subcommand("wb").unwrap();
        let cases = [
            (&root, "plans", "plan"),
            (&root, "tools", "tool"),
            (&root, "agents", "agent"),
            (&root, "threads", "thread"),
            (&root, "shapes", "shape"),
            (&root, "packs", "pack"),
            (workbench, "plans", "plan"),
            (workbench, "agents", "agent"),
            (workbench, "tools", "tool"),
        ];
        for (parent, plural, singular) in cases {
            let resolved = parent.find_subcommand(plural).map(|sub| sub.get_name());
            assert_eq!(resolved, Some(singular), "{plural}");
        }
    }

    #[test]
    fn help_lists_only_singular_command_names() {
        let root = Cli::command();
        let workbench = root.find_subcommand("wb").unwrap();
        for parent in [&root, workbench] {
            for sub in parent.get_subcommands() {
                let names = std::iter::once(sub.get_name()).chain(sub.get_visible_aliases());
                for name in names {
                    assert!(
                        !["plans", "tools", "agents", "threads", "shapes", "packs"].contains(&name),
                        "{name} is visible in help"
                    );
                }
            }
        }
    }
}
