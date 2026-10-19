//! Catalog-aware plan validation: the second validation layer.
//!
//! [`super::doc::validate_doc`] and [`Pipeline::validate_plan`] are
//! deliberately static and context-free — they run everywhere a plan is
//! parsed, including places where the runtime catalog isn't available or
//! isn't the same (the workbench). This layer takes the catalog of what is
//! actually loadable as input and resolves every step tool name against
//! it, so a plan that references a nonexistent tool fails at load/validate
//! time instead of mid-run, after earlier steps have already spent tool
//! calls and LLM money.
//!
//! What can and cannot be resolved without side effects:
//! - `builtin__*`, `user__*`, and `plan__*` names resolve exactly — those
//!   catalogs load locally.
//! - MCP `server__tool` names resolve at the *server* level only: listing
//!   a server's tools requires connecting (and spawning stdio children),
//!   which validation must never do. An unconfigured server is a hard
//!   error; the individual tool is still checked at dispatch.
//! - `workbench__*` is rejected by the static layer before this one runs.

use super::body::{control_bodies, parse_branch, Branch};
use super::doc::PlanDoc;
use super::plan::Plan;
use super::{AGENT_TOOL, ASK_TOOL, EXIT_TOOL, FILTER_TOOL, MAP_TOOL, REDUCE_TOOL, ROUTE_TOOL};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Glob matching over tool names, with `*` as "any run of characters",
/// supported anywhere in the pattern and any number of times
/// (`linear__*`, `*__search`, `linear__*_issues`, `*`). Shared by this
/// layer and the runtime resolution an `agent` step's `tools:` does.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    // A leading empty segment means the pattern starts with `*`.
    if let Some(first) = segments.first() {
        match rest.strip_prefix(first) {
            Some(tail) => rest = tail,
            None => return false,
        }
    }
    let last_index = segments.len() - 1;
    for (index, segment) in segments.iter().enumerate().skip(1) {
        if segment.is_empty() {
            continue;
        }
        if index == last_index {
            // Trailing literal must land at the very end.
            return rest.len() >= segment.len() && rest.ends_with(segment);
        }
        match rest.find(segment) {
            Some(at) => rest = &rest[at + segment.len()..],
            None => return false,
        }
    }
    true
}

/// Everything loadable a plan step can call, resolvable without
/// connecting to anything. Built from config by the CLI runtime.
#[derive(Debug, Default, Clone)]
pub struct ToolCatalog {
    /// Full namespaced names of the built-in tools (`builtin__x`).
    pub builtin_tools: BTreeSet<String>,
    /// Full namespaced names of the loadable user tools (`user__x`).
    pub user_tools: BTreeSet<String>,
    /// Identifiers of loadable plan documents (callable as `plan__<id>`).
    pub plans: BTreeSet<String>,
    /// MCP server names configured under `[mcp.*]`.
    pub mcp_servers: BTreeSet<String>,
    pub agents: BTreeSet<String>,
    pub input_schemas: BTreeMap<String, Value>,
    pub packs: crate::packs::PackSet,
}

/// The outcome of resolving a plan's tool names against a catalog.
#[derive(Debug, Default)]
pub struct CatalogCheck {
    /// Names that cannot resolve here — running the plan would fail.
    pub errors: Vec<String>,
    /// Steps using an MCP server that is declared in `requires_servers`
    /// but not configured. The plan *file* is fine — the declaration is
    /// how portable plans name their dependencies — but in this
    /// environment the plan is hidden from the catalog and cannot run.
    pub notes: Vec<String>,
}

impl CatalogCheck {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Resolve one document's step tools (control bodies included) against
/// the catalog. `requires_servers` downgrades declared-but-unconfigured
/// MCP servers from errors to notes — see [`CatalogCheck::notes`].
pub fn resolve_plan_tools(doc: &PlanDoc, catalog: &ToolCatalog) -> CatalogCheck {
    let mut check = CatalogCheck::default();
    for (step_id, tool) in step_tools(&doc.steps) {
        check_tool(&step_id, &tool, &doc.requires_servers, catalog, &mut check);
    }
    for (step_id, tool, input) in step_calls(&doc.steps) {
        check_input(&step_id, &tool, input, catalog, &mut check);
    }
    for (step_id, pattern) in agent_tool_patterns(&doc.steps) {
        check_pattern(
            &step_id,
            &pattern,
            &doc.requires_servers,
            catalog,
            &mut check,
        );
    }
    check.errors.extend(
        catalog
            .packs
            .plan_problems(&crate::packs::Origin::of(doc.pack.as_ref()), &doc.steps),
    );
    check
}

/// [`resolve_plan_tools`], following `plan__*` references into the loaded
/// catalog documents (transitively, each once) so a composed plan fails
/// before its first step rather than at the sub-plan call. Sub-plan
/// problems are prefixed with the plan they belong to.
pub fn resolve_plan_tools_deep(
    doc: &PlanDoc,
    all_docs: &[PlanDoc],
    catalog: &ToolCatalog,
) -> CatalogCheck {
    let mut check = resolve_plan_tools(doc, catalog);
    for sub in reachable_plans(&doc.steps, Some(&doc.identifier), all_docs, &catalog.packs) {
        let sub_check = resolve_plan_tools(sub, catalog);
        let prefix = |p: String| format!("plan '{}': {p}", sub.identifier);
        check
            .errors
            .extend(sub_check.errors.into_iter().map(prefix));
        check.notes.extend(sub_check.notes.into_iter().map(prefix));
    }
    check
}

pub fn reachable_plans<'a>(
    steps: &'a Plan,
    own: Option<&'a str>,
    all_docs: &'a [PlanDoc],
    packs: &crate::packs::PackSet,
) -> Vec<&'a PlanDoc> {
    let mut visited: BTreeSet<&str> = own.into_iter().collect();
    let mut reached = Vec::new();
    let mut queue: Vec<&str> = plan_refs(steps, packs);
    while let Some(identifier) = queue.pop() {
        if !visited.insert(identifier) {
            continue;
        }
        let Some(sub) = all_docs.iter().find(|d| d.identifier == identifier) else {
            continue;
        };
        queue.extend(plan_refs(&sub.steps, packs));
        reached.push(sub);
    }
    reached
}

pub fn reachable_packs(
    steps: &Plan,
    own: Option<&str>,
    all_docs: &[PlanDoc],
    packs: &crate::packs::PackSet,
) -> BTreeSet<String> {
    let docs = reachable_plans(steps, None, all_docs, packs);
    let mut reached: BTreeSet<String> = own
        .map(str::to_string)
        .into_iter()
        .chain(
            docs.iter()
                .filter_map(|doc| Some(doc.pack.as_ref()?.name.clone())),
        )
        .collect();
    for plan in std::iter::once(steps).chain(docs.iter().map(|doc| &doc.steps)) {
        let tools = step_tools(plan).into_iter().map(|(_, tool)| tool);
        let patterns = agent_tool_patterns(plan)
            .into_iter()
            .map(|(_, pattern)| pattern);
        for name in tools.chain(patterns) {
            if let Some((pack, _)) = name.split_once(crate::packs::KIND_SEPARATOR) {
                if packs.is_installed(pack) {
                    reached.insert(pack.to_string());
                }
            }
        }
    }
    reached
}

/// Resolve bare steps (no document context) — the pre-execution guard in
/// [`super::Pipeline::run_explicit`]. No `requires_servers` leniency:
/// these steps are about to run, so every unconfigured server is an error.
pub fn resolve_step_tools(plan: &Plan, catalog: &ToolCatalog) -> Vec<String> {
    let mut check = CatalogCheck::default();
    for (step_id, tool) in step_tools(plan) {
        check_tool(&step_id, &tool, &[], catalog, &mut check);
    }
    for (step_id, tool, input) in step_calls(plan) {
        check_input(&step_id, &tool, input, catalog, &mut check);
    }
    for (step_id, pattern) in agent_tool_patterns(plan) {
        check_pattern(&step_id, &pattern, &[], catalog, &mut check);
    }
    check.errors
}

/// Every `(step id, pattern)` pair from `agent` steps' `tools:` lists,
/// control-step bodies included. These are tool *references* even though
/// they live in input data, so they get resolved like any step tool name.
pub fn agent_tool_patterns(plan: &Plan) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut collect = |id: String, input: &serde_json::Map<String, serde_json::Value>| {
        let Some(serde_json::Value::Array(items)) = input.get("tools") else {
            return;
        };
        for item in items {
            if let Some(pattern) = item.as_str() {
                found.push((id.clone(), pattern.to_string()));
            }
        }
    };
    for step in plan {
        if step.tool_name == AGENT_TOOL {
            collect(step.id.clone(), &step.input);
        }
        for (key, raw) in control_bodies(&step.tool_name, &step.input) {
            let key = key.as_str();
            match parse_branch(key, raw) {
                Ok(Branch::Call(call)) if call.tool_name == AGENT_TOOL => {
                    collect(step.id.clone(), &call.input);
                }
                Ok(Branch::Steps(steps)) => {
                    for body_step in steps {
                        if body_step.tool_name == AGENT_TOOL {
                            collect(format!("{}/{}", step.id, body_step.id), &body_step.input);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// Resolve one `tools:` pattern. Mirrors [`check_tool`]'s namespace rules,
/// with the same MCP constraint: servers resolve, individual tools do not
/// (listing them would mean connecting).
fn check_pattern(
    step_id: &str,
    pattern: &str,
    requires_servers: &[String],
    catalog: &ToolCatalog,
    check: &mut CatalogCheck,
) {
    if pattern.is_empty() {
        check
            .errors
            .push(format!("step {step_id}: `tools` contains an empty pattern"));
        return;
    }
    // `*` alone means "everything", which is what omitting `tools` does.
    if pattern.chars().all(|c| c == '*') {
        return;
    }
    let Some((namespace, _)) = pattern.split_once("__") else {
        check.errors.push(format!(
            "step {step_id}: `tools` pattern '{pattern}' is not a namespaced \
             tool name or pattern (like linear__* or user__git_log)"
        ));
        return;
    };
    // A wildcard inside the namespace itself (`*__search`) can't be
    // resolved to a server without listing everything — leave it to
    // runtime, which fails on a pattern that matches nothing.
    if namespace.contains('*') {
        return;
    }
    if catalog.packs.is_installed(namespace) {
        if catalog.packs.matching(pattern).next().is_none() {
            check.errors.push(format!(
                "step {step_id}: `tools` pattern '{pattern}' matches nothing in pack '{namespace}'"
            ));
        }
        return;
    }

    let local: Option<(&BTreeSet<String>, &str)> = match namespace {
        "builtin" => Some((&catalog.builtin_tools, "builtin tool")),
        "user" => Some((&catalog.user_tools, "user tool")),
        _ => None,
    };
    if let Some((names, label)) = local {
        if !names.iter().any(|name| glob_matches(pattern, name)) {
            check.errors.push(format!(
                "step {step_id}: `tools` pattern '{pattern}' matches no {label}"
            ));
        }
        return;
    }
    if namespace == "plan" {
        let wanted = &pattern["plan__".len()..];
        if !catalog
            .plans
            .iter()
            .any(|identifier| glob_matches(wanted, identifier))
        {
            check.errors.push(format!(
                "step {step_id}: `tools` pattern '{pattern}' matches no plan in \
                 the plan catalog"
            ));
        }
        return;
    }
    if catalog.mcp_servers.contains(namespace) {
        return;
    }
    let message = format!(
        "step {step_id}: `tools` pattern '{pattern}' needs MCP server \
         '{namespace}', which is not configured under [mcp.{namespace}]"
    );
    if requires_servers.iter().any(|s| s == namespace) {
        check
            .notes
            .push(format!("{message} (declared in requires_servers)"));
    } else {
        check.errors.push(message);
    }
}

fn check_tool(
    step_id: &str,
    tool: &str,
    requires_servers: &[String],
    catalog: &ToolCatalog,
    check: &mut CatalogCheck,
) {
    match tool {
        AGENT_TOOL | ASK_TOOL | EXIT_TOOL | ROUTE_TOOL | FILTER_TOOL | MAP_TOOL | REDUCE_TOOL
        | "plan_and_execute" => {}
        _ if super::plan::workbench_tool_problem(tool).is_some() => {
            // The static layer rejects these with the full explanation;
            // repeat it here so a catalog-only caller still fails.
            check.errors.push(format!(
                "step {step_id}: {}",
                super::plan::workbench_tool_problem(tool).unwrap()
            ));
        }
        _ if catalog.packs.routed(tool).is_some() => check_pack_item(step_id, tool, catalog, check),
        _ if tool.starts_with("plan__") => {
            let identifier = &tool["plan__".len()..];
            if !catalog.plans.contains(identifier) {
                check.errors.push(format!(
                    "step {step_id}: unknown plan '{identifier}' ({tool}) — not \
                     in the plan catalog (missing, failed to load, or hidden by \
                     an unconfigured requires_servers entry)"
                ));
            }
        }
        _ if tool.starts_with(super::AGENT_TOOL_PREFIX) => {
            let name = &tool[super::AGENT_TOOL_PREFIX.len()..];
            if !catalog.agents.contains(name) {
                check.errors.push(format!(
                    "step {step_id}: unknown agent '{name}' ({tool}) — no agent with \
                     that name is built in or defined under an agents directory"
                ));
            }
        }
        _ if tool.starts_with(crate::user_tools::BUILTIN_TOOL_PREFIX) => {
            if !catalog.builtin_tools.contains(tool) && !super::is_native_tool(tool) {
                let bare = &tool[crate::user_tools::BUILTIN_TOOL_PREFIX.len()..];
                check.errors.push(match moved_pack_of(bare) {
                    Some(pack) => format!(
                        "step {step_id}: unknown builtin tool '{tool}' — the {pack} tools \
                         left graph for the {pack} pack: install it with `graph pack add {pack}` \
                         and call {pack}__tool__{bare}"
                    ),
                    None => format!(
                        "step {step_id}: unknown builtin tool '{tool}' — not one of graph's \
                         built-in tools"
                    ),
                });
            }
        }
        _ if tool.starts_with(crate::user_tools::USER_TOOL_PREFIX) => {
            if !catalog.user_tools.contains(tool) {
                check.errors.push(format!(
                    "step {step_id}: unknown user tool '{tool}' — no such tool \
                     loads from [tools].paths"
                ));
            }
        }
        _ => {
            // MCP `server__tool`: verify the server; the tool itself is
            // only knowable by connecting. Bare non-control names are the
            // static layer's problem — skip them here.
            let Some((server, _)) = tool.split_once("__") else {
                return;
            };
            if catalog.mcp_servers.contains(server) {
                return;
            }
            let message = format!(
                "step {step_id}: tool '{tool}' needs MCP server '{server}', \
                 which is not configured under [mcp.{server}]"
            );
            if requires_servers.iter().any(|s| s == server) {
                check
                    .notes
                    .push(format!("{message} (declared in requires_servers)"));
            } else {
                check.errors.push(message);
            }
        }
    }
}

fn check_pack_item(step_id: &str, tool: &str, catalog: &ToolCatalog, check: &mut CatalogCheck) {
    let Some((pack, kind, name)) = catalog.packs.routed(tool) else {
        return;
    };
    if catalog.packs.item(tool).is_none() {
        check.errors.push(format!(
            "step {step_id}: unknown {kind} '{tool}' — pack '{pack}' has no {kind} named '{name}'",
            kind = kind.as_str()
        ));
    }
}

fn moved_pack_of(bare: &str) -> Option<&'static str> {
    if bare.starts_with("gh_") || bare.starts_with("git_") {
        Some("github")
    } else if bare.starts_with("slack_") {
        Some("slack")
    } else {
        None
    }
}

/// Every `(step id, tool name)` pair in a plan, control-step bodies
/// included. Bodies that fail to parse are skipped — the static layer
/// reports those.
pub fn step_tools(plan: &Plan) -> Vec<(String, String)> {
    step_calls(plan)
        .into_iter()
        .map(|(id, tool, _)| (id, tool))
        .collect()
}

pub(crate) fn step_calls(plan: &Plan) -> Vec<(String, String, Map<String, Value>)> {
    let mut calls = Vec::new();
    for step in plan {
        calls.push((step.id.clone(), step.tool_name.clone(), step.input.clone()));
        for (key, raw) in control_bodies(&step.tool_name, &step.input) {
            let key = key.as_str();
            match parse_branch(key, raw) {
                Ok(Branch::Call(call)) => calls.push((step.id.clone(), call.tool_name, call.input)),
                Ok(Branch::Steps(steps)) => {
                    for body_step in steps {
                        calls.push((
                            format!("{}/{}", step.id, body_step.id),
                            body_step.tool_name,
                            body_step.input,
                        ));
                    }
                }
                Err(_) => {}
            }
        }
    }
    calls
}

fn check_input(
    step_id: &str,
    tool: &str,
    input: Map<String, Value>,
    catalog: &ToolCatalog,
    check: &mut CatalogCheck,
) {
    let Some(schema) = catalog.input_schemas.get(tool) else {
        return;
    };
    check.errors.extend(
        input_problems(tool, input, schema)
            .into_iter()
            .map(|problem| format!("step {step_id}: {problem}")),
    );
}

pub(crate) fn input_problems(tool: &str, input: Map<String, Value>, schema: &Value) -> Vec<String> {
    let Ok(validator) = jsonschema::validator_for(schema) else {
        return Vec::new();
    };
    let mut input = Value::Object(input);
    super::doc::apply_schema_defaults(schema, &mut input);
    validator
        .iter_errors(&input)
        .filter_map(|error| {
            let path = error.instance_path.to_string();
            if templated_along(&input, &path) {
                return None;
            }
            let at = if path.is_empty() {
                String::new()
            } else {
                format!(" at {path}")
            };
            Some(format!("input for {tool}{at}: {error}"))
        })
        .collect()
}

fn templated_along(input: &Value, path: &str) -> bool {
    let is_templated = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("{{"))
    };
    let mut prefix = String::new();
    for segment in path.split('/').skip(1) {
        prefix.push('/');
        prefix.push_str(segment);
        if is_templated(input.pointer(&prefix)) {
            return true;
        }
    }
    false
}

/// The distinct `plan__*` identifiers a plan's steps (bodies included)
/// reference.
fn plan_refs<'a>(plan: &'a Plan, packs: &crate::packs::PackSet) -> Vec<&'a str> {
    let mut refs = Vec::new();
    for step in plan {
        if let Some(identifier) = packs.plan_target(&step.tool_name) {
            refs.push(identifier);
        }
        for (_, raw) in control_bodies(&step.tool_name, &step.input) {
            collect_plan_refs(raw, packs, &mut refs);
        }
    }
    refs
}

fn collect_plan_refs<'a>(
    raw: &'a serde_json::Value,
    packs: &crate::packs::PackSet,
    refs: &mut Vec<&'a str>,
) {
    let tool_of = |value: &'a serde_json::Value| {
        value
            .get("toolName")
            .or_else(|| value.get("tool_name"))
            .and_then(serde_json::Value::as_str)
    };
    match raw {
        serde_json::Value::Object(_) => {
            if let Some(identifier) = tool_of(raw).and_then(|t| packs.plan_target(t)) {
                refs.push(identifier);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(identifier) = tool_of(item).and_then(|t| packs.plan_target(t)) {
                    refs.push(identifier);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_wildcards_anywhere() {
        assert!(glob_matches("linear__*", "linear__list_issues"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*__search", "brave__search"));
        assert!(glob_matches("linear__*_issues", "linear__list_issues"));
        assert!(glob_matches("user__git_log", "user__git_log"));

        // A prefix-only implementation gets all of these wrong.
        assert!(!glob_matches("*__search", "brave__search_web"));
        assert!(!glob_matches("linear__*_issues", "linear__list_teams"));
        assert!(!glob_matches("user__git_log", "user__git_log_extra"));
        assert!(!glob_matches("linear__*", "user__git_log"));
    }

    fn catalog() -> ToolCatalog {
        ToolCatalog {
            builtin_tools: BTreeSet::from(["builtin__infer".to_string()]),
            user_tools: BTreeSet::from(["user__git_log".to_string()]),
            plans: BTreeSet::from(["urgent_issues".to_string()]),
            mcp_servers: BTreeSet::from(["linear".to_string()]),
            agents: BTreeSet::from(["outliner".to_string()]),
            ..Default::default()
        }
    }

    fn doc(yaml: &str) -> PlanDoc {
        str::parse::<crate::pipeline::doc::PlanDoc>(yaml).unwrap()
    }

    const OK_DOC: &str = r#"
identifier: demo
name: Demo
description: demo
requires_servers: [linear]
steps:
  - id: E0
    tool_name: linear__list_issues
    input: {}
  - id: E1
    tool_name: builtin__infer
    input: { instruction: x, data: "{{E0}}" }
  - id: E2
    tool_name: user__git_log
    input: {}
  - id: E3
    tool_name: plan__urgent_issues
    input: {}
  - id: E4
    tool_name: exit
    input: { status: success, message: done }
"#;

    #[test]
    fn resolvable_names_in_every_namespace_pass() {
        let check = resolve_plan_tools(&doc(OK_DOC), &catalog());
        assert!(check.is_ok(), "{:?}", check.errors);
        assert!(check.notes.is_empty(), "{:?}", check.notes);
    }

    #[test]
    fn unknown_names_error_per_namespace() {
        let bad = OK_DOC
            .replace("builtin__infer", "builtin__nope")
            .replace("user__git_log", "user__nope")
            .replace("plan__urgent_issues", "plan__nope");
        let check = resolve_plan_tools(&doc(&bad), &catalog());
        assert_eq!(check.errors.len(), 3, "{:?}", check.errors);
        assert!(check.errors[0].contains("unknown builtin tool 'builtin__nope'"));
        assert!(check.errors[0].contains("not one of graph's built-in tools"));
        assert!(check.errors[1].contains("unknown user tool 'user__nope'"));
        assert!(check.errors[2].contains("unknown plan 'nope'"));
    }

    #[test]
    fn a_former_github_or_slack_builtin_names_the_pack_to_install() {
        let moved = OK_DOC.replace("builtin__infer", "builtin__git_log");
        let check = resolve_plan_tools(&doc(&moved), &catalog());
        assert_eq!(
            check.errors,
            ["step E1: unknown builtin tool 'builtin__git_log' — the github tools left graph for the github pack: install it with `graph pack add github` and call github__tool__git_log"]
        );
    }

    #[test]
    fn unconfigured_undeclared_server_is_an_error() {
        let bad = OK_DOC.replace("linear__list_issues", "github__search_issues");
        let check = resolve_plan_tools(&doc(&bad), &catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(
            check.errors[0].contains("MCP server 'github'"),
            "{}",
            check.errors[0]
        );
        assert!(check.errors[0].contains("[mcp.github]"));
    }

    #[test]
    fn declared_but_unconfigured_server_is_a_note_not_an_error() {
        let declared = OK_DOC
            .replace("requires_servers: [linear]", "requires_servers: [github]")
            .replace("linear__list_issues", "github__search_issues");
        let check = resolve_plan_tools(&doc(&declared), &catalog());
        assert!(check.is_ok(), "{:?}", check.errors);
        assert_eq!(check.notes.len(), 1, "{:?}", check.notes);
        assert!(check.notes[0].contains("declared in requires_servers"));
    }

    #[test]
    fn workbench_tools_are_rejected() {
        let bad = OK_DOC.replace("linear__list_issues", "workbench__read_file");
        let check = resolve_plan_tools(&doc(&bad), &catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(
            check.errors[0].contains("workbench__"),
            "{:?}",
            check.errors
        );
        assert!(check.errors[0].contains("not available in the plan runtime"));
    }

    #[test]
    fn control_step_bodies_are_resolved_too() {
        let body_doc = doc(r#"
identifier: bodies
name: Bodies
description: control bodies
steps:
  - id: E0
    tool_name: linear__list_issues
    input: {}
  - id: E1
    tool_name: route
    input:
      if: { value: "{{E0.count}}", op: gt, to: 0 }
      then: { toolName: ghost__triage, input: {} }
      else:
        - id: B0
          toolName: user__nope
          input: {}
  - id: E2
    tool_name: map
    input:
      over: "{{E0.issues}}"
      do: { toolName: builtin__nope, input: {} }
"#);
        let check = resolve_plan_tools(&body_doc, &catalog());
        assert_eq!(check.errors.len(), 3, "{:?}", check.errors);
        assert!(check.errors[0].contains("ghost"), "{}", check.errors[0]);
        assert!(check.errors[1].contains("E1/B0"), "{}", check.errors[1]);
        assert!(
            check.errors[2].contains("builtin__nope"),
            "{}",
            check.errors[2]
        );
    }

    #[test]
    fn agent_tool_patterns_resolve_against_the_catalog() {
        // The exact case that shipped a "valid" plan that could not run:
        // linear__* with no [mcp.linear] configured.
        let d = doc(r#"
identifier: demo
name: Demo
description: d
steps:
  - id: E0
    tool_name: agent
    input:
      prompt: hi
      outputSchema: { type: object, properties: {} }
      tools: ["linear__*"]
"#);
        let mut empty = catalog();
        empty.mcp_servers = BTreeSet::default();
        let check = resolve_plan_tools(&d, &empty);
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(check.errors[0].contains("linear__*"), "{}", check.errors[0]);
        assert!(
            check.errors[0].contains("[mcp.linear]"),
            "{}",
            check.errors[0]
        );

        // Configured server -> fine.
        assert!(resolve_plan_tools(&d, &catalog()).is_ok());
    }

    #[test]
    fn agent_tool_patterns_resolve_local_namespaces_exactly() {
        let d = doc(r#"
identifier: demo
name: Demo
description: d
steps:
  - id: E0
    tool_name: agent
    input:
      prompt: hi
      outputSchema: { type: object, properties: {} }
      tools: ["user__nope", "builtin__*", "plan__ghost", "notnamespaced", "user__git_log"]
"#);
        let check = resolve_plan_tools(&d, &catalog());
        let joined = check.errors.join(" | ");
        assert!(joined.contains("user__nope"), "{joined}");
        assert!(joined.contains("plan__ghost"), "{joined}");
        assert!(joined.contains("notnamespaced"), "{joined}");
        // builtin__* matches builtin__infer and user__git_log is real, so
        // neither may be flagged. Match on the quoted pattern form so the
        // "(like linear__* or user__git_log)" hint text can't false-match.
        assert!(!joined.contains("'builtin__*'"), "{joined}");
        assert!(!joined.contains("'user__git_log'"), "{joined}");
        assert_eq!(check.errors.len(), 3, "{:?}", check.errors);
    }

    #[test]
    fn agent_patterns_inside_control_bodies_are_resolved() {
        let d = doc(r#"
identifier: demo
name: Demo
description: d
steps:
  - id: E0
    tool_name: user__git_log
    input: {}
  - id: E1
    tool_name: map
    input:
      over: "{{E0.commits}}"
      do:
        toolName: agent
        input:
          prompt: "{{item}}"
          outputSchema: { type: object, properties: {} }
          tools: ["ghost__thing"]
  - id: E2
    tool_name: route
    input:
      if: { value: "{{E0.count}}", op: gt, to: 0 }
      then:
        - id: B0
          toolName: agent
          input:
            prompt: x
            outputSchema: { type: object, properties: {} }
            tools: ["user__missing"]
"#);
        let check = resolve_plan_tools(&d, &catalog());
        let joined = check.errors.join(" | ");
        assert!(joined.contains("ghost"), "{joined}");
        assert!(joined.contains("user__missing"), "{joined}");
        assert!(joined.contains("E2/B0"), "{joined}");
    }

    #[test]
    fn declared_but_unconfigured_server_in_agent_patterns_is_a_note() {
        let d = doc(r#"
identifier: demo
name: Demo
description: d
requires_servers: [github]
steps:
  - id: E0
    tool_name: agent
    input:
      prompt: hi
      outputSchema: { type: object, properties: {} }
      tools: ["github__*"]
"#);
        let check = resolve_plan_tools(&d, &catalog());
        assert!(check.is_ok(), "{:?}", check.errors);
        assert_eq!(check.notes.len(), 1, "{:?}", check.notes);
    }

    #[test]
    fn deep_resolution_follows_plan_references() {
        let sub = doc(r#"
identifier: urgent_issues
name: Urgent
description: sub-plan with a defect
steps:
  - id: E0
    tool_name: ghost__list
    input: {}
"#);
        let check = resolve_plan_tools_deep(&doc(OK_DOC), &[sub], &catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(
            check.errors[0].starts_with("plan 'urgent_issues':"),
            "{}",
            check.errors[0]
        );
        assert!(check.errors[0].contains("ghost"));
    }

    #[test]
    fn deep_resolution_survives_plan_cycles() {
        let a = doc(r#"
identifier: a
name: A
description: calls b
steps:
  - id: E0
    tool_name: plan__b
    input: {}
"#);
        let b = doc(r#"
identifier: b
name: B
description: calls a
steps:
  - id: E0
    tool_name: plan__a
    input: {}
"#);
        let mut catalog = catalog();
        catalog.plans.extend(["a".to_string(), "b".to_string()]);
        let check = resolve_plan_tools_deep(&a, &[a.clone(), b], &catalog);
        assert!(check.is_ok(), "{:?}", check.errors);
    }

    #[test]
    fn step_tool_resolution_has_no_requires_servers_leniency() {
        let declared = OK_DOC
            .replace("requires_servers: [linear]", "requires_servers: [github]")
            .replace("linear__list_issues", "github__search_issues");
        let problems = resolve_step_tools(&doc(&declared).steps, &catalog());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("MCP server 'github'"));
    }

    fn schema_catalog() -> ToolCatalog {
        let mut catalog = catalog();
        catalog.input_schemas.insert(
            "user__git_log".to_string(),
            serde_json::json!({
                "type": "object",
                "required": ["repo"],
                "properties": {
                    "repo": { "type": "string" },
                    "count": { "type": "integer", "default": 10 },
                    "filter": {
                        "type": "object",
                        "properties": { "author": { "type": "string" } }
                    }
                }
            }),
        );
        catalog
    }

    fn input_doc(input: &str) -> PlanDoc {
        doc(&format!(
            r#"
identifier: demo
name: Demo
description: demo
steps:
  - id: E0
    tool_name: user__git_log
    input: {input}
"#
        ))
    }

    #[test]
    fn a_missing_required_input_is_reported_with_step_and_tool() {
        let check = resolve_plan_tools(&input_doc("{ count: 3 }"), &schema_catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(check.errors[0].starts_with("step E0: input for user__git_log"));
        assert!(check.errors[0].contains("\"repo\" is a required property"));
    }

    #[test]
    fn a_literal_of_the_wrong_type_is_reported_at_its_path() {
        let check = resolve_plan_tools(
            &input_doc("{ repo: ., count: three, filter: { author: 4 } }"),
            &schema_catalog(),
        );
        assert_eq!(check.errors.len(), 2, "{:?}", check.errors);
        assert!(check.errors.iter().any(|e| e.contains(" at /count:")));
        assert!(check
            .errors
            .iter()
            .any(|e| e.contains(" at /filter/author:")));
    }

    #[test]
    fn templated_values_count_as_present_and_are_never_type_checked() {
        let check = resolve_plan_tools(
            &input_doc(
                r#"{ repo: "{{input.repo}}", count: "{{input.count}}", filter: "{{E9.filter}}" }"#,
            ),
            &schema_catalog(),
        );
        assert!(check.is_ok(), "{:?}", check.errors);
    }

    #[test]
    fn schema_defaults_fill_required_inputs_before_checking() {
        let mut catalog = schema_catalog();
        let schema = catalog.input_schemas.get_mut("user__git_log").unwrap();
        schema["properties"]["repo"]["default"] = serde_json::json!(".");
        let check = resolve_plan_tools(&input_doc("{}"), &catalog);
        assert!(check.is_ok(), "{:?}", check.errors);
    }

    #[test]
    fn undeclared_keys_pass_unless_the_schema_forbids_them() {
        let open = resolve_plan_tools(&input_doc("{ repo: ., extra: 1 }"), &schema_catalog());
        assert!(open.is_ok(), "{:?}", open.errors);

        let mut catalog = schema_catalog();
        let schema = catalog.input_schemas.get_mut("user__git_log").unwrap();
        schema["additionalProperties"] = serde_json::json!(false);
        let closed = resolve_plan_tools(&input_doc("{ repo: ., extra: 1 }"), &catalog);
        assert_eq!(closed.errors.len(), 1, "{:?}", closed.errors);
    }

    #[test]
    fn body_step_inputs_are_checked_with_their_body_path() {
        let plan = doc(r#"
identifier: demo
name: Demo
description: demo
steps:
  - id: E0
    tool_name: map
    input:
      over: "{{input.repos}}"
      do:
        - id: log
          tool_name: user__git_log
          input: { count: 2 }
"#);
        let check = resolve_plan_tools(&plan, &schema_catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(check.errors[0].starts_with("step E0/log: input for user__git_log"));
    }

    #[test]
    fn tools_without_a_known_schema_are_not_input_checked() {
        let check = resolve_plan_tools(&doc(OK_DOC), &schema_catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(check.errors[0].contains("user__git_log"));
    }

    #[test]
    fn step_resolution_reports_input_problems_before_a_run() {
        let problems = resolve_step_tools(&input_doc("{ count: 3 }").steps, &schema_catalog());
        assert_eq!(problems.len(), 1, "{problems:?}");
    }
}
