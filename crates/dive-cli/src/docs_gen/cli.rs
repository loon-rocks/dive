use super::mdx::{frontmatter, paragraphs, text_line as mdx_text};
use super::Section;
use crate::cli::Cli;
use clap::{Arg, ArgAction, Command, CommandFactory};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

const PAGE_DIR: &str = "reference/cli";
const NAV_FILE: &str = "reference/cli/navigation.json";

pub fn section() -> Section {
    Section {
        dir: Some(PAGE_DIR),
        nav: Some(NAV_FILE.to_string()),
        hand_written: &["reference/cli/error-handling.mdx"],
        files: render_pages(),
    }
}

fn render_pages() -> BTreeMap<String, String> {
    let root = built_root();
    let mut pages = BTreeMap::new();
    pages.insert(format!("{PAGE_DIR}/overview.mdx"), overview_page(&root));
    for command in runnable_descendants(&root) {
        pages.insert(format!("{}.mdx", page_path(command)), command_page(command));
    }
    for command in group_descendants(&root) {
        if !is_runnable(command) {
            pages.insert(format!("{}.mdx", page_path(command)), group_page(command));
        }
    }
    pages.insert(NAV_FILE.to_string(), navigation(&root));
    pages
}

pub fn missing_help() -> Vec<String> {
    let root = built_root();
    let mut missing = Vec::new();
    collect_missing_help(&root, &mut missing);
    missing
}

fn built_root() -> Command {
    let mut root = Cli::command();
    root.build();
    root
}

fn collect_missing_help(command: &Command, missing: &mut Vec<String>) {
    let path = bin_name(command);
    if command.get_about().is_none() {
        missing.push(format!("`{path}` has no about text"));
    }
    for arg in own_args(command) {
        if arg.get_help().is_none() && arg.get_long_help().is_none() {
            missing.push(format!(
                "`{path}` argument `{}` has no help",
                arg_label(arg)
            ));
        }
    }
    for sub in visible_subcommands(command) {
        collect_missing_help(sub, missing);
    }
}

fn navigation(root: &Command) -> String {
    let mut pages = Vec::new();
    for command in visible_subcommands(root) {
        pages.push(nav_entry(command));
    }
    pages.push(json!(format!("{PAGE_DIR}/error-handling")));
    let mut out = serde_json::to_string_pretty(&Value::Array(pages)).unwrap();
    out.push('\n');
    out
}

fn nav_entry(command: &Command) -> Value {
    let subs: Vec<&Command> = visible_subcommands(command).collect();
    if subs.is_empty() {
        return json!(page_path(command));
    }
    let pages: Vec<Value> = subs.into_iter().map(nav_entry).collect();
    json!({ "group": bin_name(command), "root": page_path(command), "pages": pages })
}

fn group_descendants(command: &Command) -> Vec<&Command> {
    let mut found = Vec::new();
    for sub in visible_subcommands(command) {
        if visible_subcommands(sub).next().is_some() {
            found.push(sub);
        }
        found.extend(group_descendants(sub));
    }
    found
}

fn available_commands(out: &mut String, command: &Command) {
    let subs: Vec<&Command> = visible_subcommands(command).collect();
    if subs.is_empty() {
        return;
    }
    out.push_str("## Available commands\n\n");
    for sub in subs {
        let about = sub
            .get_about()
            .map(|about| mdx_text(&about.to_string()))
            .unwrap_or_default();
        let _ = writeln!(out, "- [`{}`](/{}): {about}", bin_name(sub), page_path(sub));
    }
    out.push('\n');
}

fn group_page(command: &Command) -> String {
    let mut out = String::new();
    let path = bin_name(command);
    let about = command
        .get_about()
        .map(|about| about.to_string())
        .unwrap_or_default();
    frontmatter(
        &mut out,
        &[
            ("title", path.clone()),
            ("sidebarTitle", path.clone()),
            ("description", about),
            ("playground", "none".to_string()),
        ],
        &[path.as_str()],
    );
    available_commands(&mut out, command);
    arg_section(&mut out, "Global options", global_args(command).into_iter());
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

fn overview_page(root: &Command) -> String {
    let mut out = String::new();
    frontmatter(
        &mut out,
        &[
            ("title", "CLI reference".to_string()),
            (
                "description",
                "Every graph command, generated from the CLI's own definitions.".to_string(),
            ),
        ],
        &["cli", "commands", "flags", "options"],
    );
    if let Some(about) = root.get_about() {
        let _ = writeln!(out, "{}\n", mdx_text(&about.to_string()));
    }
    let _ = writeln!(out, "```bash\n{}\n```\n", usage(root));
    out.push_str("## Global options\n\nThese options work with every command.\n\n");
    for arg in global_args(root) {
        param_field(&mut out, arg);
    }
    out.push_str("## Commands\n\n| Command | Description |\n| --- | --- |\n");
    for command in runnable_descendants(root) {
        let about = command
            .get_about()
            .map(|about| mdx_text(&about.to_string()))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "| [`{}`](/{}) | {about} |",
            bin_name(command),
            page_path(command)
        );
    }
    let _ = writeln!(
        out,
        "\nExit codes and output streams are covered in [Error handling](/{PAGE_DIR}/error-handling)."
    );
    out
}

fn command_page(command: &Command) -> String {
    let mut out = String::new();
    let path = bin_name(command);
    let about = command
        .get_about()
        .map(|about| about.to_string())
        .unwrap_or_default();
    let sidebar = if path.split_whitespace().count() > 2 {
        command.get_name().to_string()
    } else {
        path.clone()
    };
    frontmatter(
        &mut out,
        &[
            ("title", path.clone()),
            ("sidebarTitle", sidebar),
            ("description", about),
            ("playground", "none".to_string()),
        ],
        &[path.as_str()],
    );
    let (details, examples) = after_help(command);
    if !details.is_empty() {
        let _ = writeln!(out, "{}\n", paragraphs(&details));
    }
    let aliases: Vec<String> = command
        .get_visible_aliases()
        .map(|alias| format!("`{}`", path.replacen(command.get_name(), alias, 1)))
        .collect();
    if !aliases.is_empty() {
        let _ = writeln!(out, "Alias: {}.\n", aliases.join(", "));
    }
    let args: Vec<&Arg> = own_args(command).collect();
    arg_section(
        &mut out,
        "Arguments",
        args.iter().copied().filter(|arg| arg.is_positional()),
    );
    arg_section(
        &mut out,
        "Options",
        args.iter().copied().filter(|arg| !arg.is_positional()),
    );
    available_commands(&mut out, command);
    arg_section(&mut out, "Global options", global_args(command).into_iter());
    out.push_str("<RequestExample>\n\n");
    let _ = writeln!(out, "```bash Usage\n{}\n```\n", usage(command));
    for (index, example) in examples.iter().enumerate() {
        let title = if examples.len() == 1 {
            "Example".to_string()
        } else {
            format!("Example {}", index + 1)
        };
        let _ = writeln!(out, "```bash {title}\n{example}\n```\n");
    }
    out.push_str("</RequestExample>\n");
    out
}

fn arg_section<'a>(out: &mut String, heading: &str, args: impl Iterator<Item = &'a Arg>) {
    let args: Vec<&Arg> = args.collect();
    if args.is_empty() {
        return;
    }
    let _ = writeln!(out, "## {heading}\n");
    for arg in args {
        param_field(out, arg);
    }
}

fn param_field(out: &mut String, arg: &Arg) {
    let location = if arg.is_positional() { "path" } else { "query" };
    let _ = write!(
        out,
        "<ParamField {location}=\"{}\" type=\"{}\"",
        arg_label(arg),
        value_type(arg)
    );
    if arg.is_required_set() {
        out.push_str(" required");
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if !defaults.is_empty() && !is_flag(arg) {
        let _ = write!(out, " default=\"{}\"", defaults.join(" "));
    }
    out.push_str(">\n");
    let help = arg
        .get_long_help()
        .or(arg.get_help())
        .map(|help| help.to_string())
        .unwrap_or_default();
    let mut body = vec![paragraphs(&help)];
    if !arg.is_positional() && !is_flag(arg) && arg.get_value_names().is_some() {
        body.push(format!("Format: `{}`.", value_name(arg)));
    }
    if takes_optional_value(arg) {
        body.push(format!(
            "The value is optional: `--{}` on its own is accepted.",
            arg.get_long().unwrap_or_default()
        ));
    }
    let values = possible_values(arg);
    if !values.is_empty() {
        body.push(format!("Available options: {}", values.join(", ")));
    }
    for line in body.join("\n\n").lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "  {line}");
        }
    }
    out.push_str("</ParamField>\n\n");
}

fn value_type(arg: &Arg) -> String {
    if matches!(arg.get_action(), ArgAction::Count) {
        return "integer".to_string();
    }
    if is_flag(arg) {
        return "boolean".to_string();
    }
    let base = if possible_values(arg).is_empty() {
        "string"
    } else {
        "enum<string>"
    };
    if is_repeatable(arg) {
        format!("{base}[]")
    } else {
        base.to_string()
    }
}

fn possible_values(arg: &Arg) -> Vec<String> {
    if is_flag(arg) {
        return Vec::new();
    }
    arg.get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| format!("`{}`", value.get_name()))
        .collect()
}

fn after_help(command: &Command) -> (String, Vec<String>) {
    let text = command
        .get_after_long_help()
        .map(|after| after.to_string())
        .unwrap_or_default();
    let (details, examples) = match text.split_once("Examples:") {
        Some((details, examples)) => (details, examples),
        None => (text.as_str(), ""),
    };
    let examples = examples
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    (details.trim().to_string(), examples)
}

fn usage(command: &Command) -> String {
    let rendered = command.clone().render_usage().to_string();
    rendered
        .strip_prefix("Usage: ")
        .unwrap_or(&rendered)
        .trim()
        .to_string()
}

fn bin_name(command: &Command) -> String {
    command
        .get_bin_name()
        .unwrap_or(command.get_name())
        .to_string()
}

fn page_path(command: &Command) -> String {
    let path = bin_name(command);
    let segments: Vec<&str> = path.split_whitespace().skip(1).collect();
    format!("{PAGE_DIR}/{}", segments.join("/"))
}

fn visible_subcommands(command: &Command) -> impl Iterator<Item = &Command> {
    command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
}

fn runnable_descendants(command: &Command) -> Vec<&Command> {
    let mut found = Vec::new();
    for sub in visible_subcommands(command) {
        if is_runnable(sub) {
            found.push(sub);
        }
        found.extend(runnable_descendants(sub));
    }
    found
}

fn is_runnable(command: &Command) -> bool {
    !command.has_subcommands() || !command.is_subcommand_required_set()
}

fn own_args(command: &Command) -> impl Iterator<Item = &Arg> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && !arg.is_global_set() && !is_builtin(arg))
}

fn global_args(command: &Command) -> Vec<&Arg> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && arg.is_global_set())
        .collect()
}

fn is_builtin(arg: &Arg) -> bool {
    arg.get_id() == "help" || arg.get_id() == "version"
}

fn arg_label(arg: &Arg) -> String {
    if arg.is_positional() {
        return value_name(arg);
    }
    let mut label = String::new();
    if let Some(short) = arg.get_short() {
        let _ = write!(label, "-{short}");
        if arg.get_long().is_some() {
            label.push_str(", ");
        }
    }
    if let Some(long) = arg.get_long() {
        let _ = write!(label, "--{long}");
    }
    label
}

fn value_name(arg: &Arg) -> String {
    arg.get_value_names()
        .map(|names| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| arg.get_id().to_string().to_uppercase())
}

fn is_flag(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::SetTrue
            | ArgAction::SetFalse
            | ArgAction::Count
            | ArgAction::Help
            | ArgAction::HelpShort
            | ArgAction::HelpLong
            | ArgAction::Version
    )
}

fn is_repeatable(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Append)
        || arg
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1)
}

fn takes_optional_value(arg: &Arg) -> bool {
    !is_flag(arg)
        && !arg.is_positional()
        && arg
            .get_num_args()
            .is_some_and(|range| range.min_values() == 0 && range.max_values() == 1)
}
