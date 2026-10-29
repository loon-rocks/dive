use super::InstalledPacks;
use crate::runtime::{Runtime, RuntimeOptions};
use anyhow::Result;
use dive_config::pack::{load_manifest, LayerKind, MANIFEST_FILE, SUPPORTED_OS};
use dive_config::Config;
use dive_core::agent::doc::{AgentDoc, AgentSet, AgentSource, BUILTINS};
use dive_core::packs::{
    catalog_name, inspect_pack, literal_self_names, requirements, split_catalog_name, ItemKind,
    LoadedPack, Origin, PackSource, SELF_PREFIX,
};
use dive_core::pipeline::catalog::{
    resolve_plan_tools, resolve_step_tools, step_tools, ToolCatalog,
};
use dive_core::pipeline::doc::{load_plan_docs, PlanDoc};
use dive_core::pipeline::{Finish, Step, AGENT_TOOL};
use dive_core::{EventSink, Store, ToolRegistry};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MIGRATION_PREFIX: &str = "migrate_v";
pub const MIGRATION_INPUTS: [&str; 2] = ["root", "apply"];
pub const MIGRATION_TESTS: &str = "tests/migrations";

pub fn migration_major(name: &str) -> Option<u64> {
    major_number(name.strip_prefix(MIGRATION_PREFIX)?)
}

fn major_number(digits: &str) -> Option<u64> {
    let canonical = !digits.is_empty()
        && !digits.starts_with('0')
        && digits.chars().all(|c| c.is_ascii_digit());
    canonical.then(|| digits.parse().ok()).flatten()
}

pub fn source_at(dir: &Path) -> PackSource {
    let name = load_manifest(&dir.join(MANIFEST_FILE))
        .map(|manifest| manifest.name)
        .unwrap_or_else(|_| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("pack")
                .to_string()
        });
    PackSource {
        name,
        tag: None,
        layer: LayerKind::Project,
        dir: dir.to_path_buf(),
    }
}

pub fn inspect(source: PackSource) -> (LoadedPack, Vec<String>) {
    let (mut loaded, mut problems) = inspect_pack(source, &[], &[]);
    problems.extend(loaded.load_migrations());
    (loaded, problems)
}

pub fn load_with_migrations(source: PackSource) -> Result<LoadedPack, Vec<String>> {
    let (loaded, problems) = inspect(source);
    if problems.is_empty() {
        Ok(loaded)
    } else {
        Err(problems)
    }
}

pub fn migrations(loaded: &LoadedPack) -> Vec<u64> {
    let mut majors: Vec<u64> = loaded
        .migrations
        .iter()
        .filter_map(|name| match split_catalog_name(name)? {
            (_, ItemKind::Plan, bare) => migration_major(bare),
            _ => None,
        })
        .collect();
    majors.sort_unstable();
    majors
}

pub struct PackSession {
    runtime: Runtime,
    store: Arc<dyn Store>,
    events: Arc<dyn EventSink>,
    pack: LoadedPack,
}

impl PackSession {
    pub fn open(
        config: &Config,
        options: &RuntimeOptions,
        pack: LoadedPack,
        events: Arc<dyn EventSink>,
    ) -> Result<Self> {
        let runtime = isolated_runtime(config, options, None, pack.clone())?;
        let store = runtime.store()?;
        Ok(PackSession {
            runtime,
            store,
            events,
            pack,
        })
    }

    pub fn resolve(&self, item: &str) -> Result<String, String> {
        let pack = &self.pack.source.name;
        let local = item.strip_prefix(SELF_PREFIX).unwrap_or(item);
        let named = match split_catalog_name(local) {
            Some((owner, _, _)) if owner == pack => Some(local.to_string()),
            _ => local.split_once("__").and_then(|(kind, name)| {
                ItemKind::parse(kind).map(|kind| catalog_name(pack, kind, name))
            }),
        };
        let known: Vec<String> = self
            .pack
            .items
            .iter()
            .map(|found| found.catalog_name())
            .collect();
        let found = match named {
            Some(name) => known.iter().find(|known| **known == name).cloned(),
            None => {
                let matches: Vec<&String> = self
                    .pack
                    .items
                    .iter()
                    .zip(&known)
                    .filter(|(found, _)| found.name == local)
                    .map(|(_, name)| name)
                    .collect();
                match matches.as_slice() {
                    [only] => Some((*only).clone()),
                    [] => None,
                    several => {
                        return Err(format!(
                            "'{item}' names several items of pack '{pack}': {}; say which with <tool|plan|agent>__{local}",
                            several
                                .iter()
                                .map(|name| name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    }
                }
            }
        };
        found.ok_or_else(|| {
            format!(
                "pack '{pack}' has no item '{item}'; its items are {}",
                known.join(", ")
            )
        })
    }

    pub async fn run(&self, item: &str, input: Value) -> Result<Value, String> {
        let tool = self.resolve(item)?;
        let Value::Object(fields) = &input else {
            return Err(format!("input must be a JSON object, got {input}"));
        };
        let step_input: Map<String, Value> = fields
            .iter()
            .map(|(key, value)| {
                let passed = if is_identifier(key) {
                    Value::String(format!("{{{{input.{key}}}}}"))
                } else {
                    value.clone()
                };
                (key.clone(), passed)
            })
            .collect();
        let step = Step {
            id: "run".to_string(),
            tool_name: tool.clone(),
            input: step_input,
            reasoning: None,
            files: None,
        };
        let pipeline = self
            .runtime
            .pipeline(&self.store, self.events.clone())
            .await
            .map_err(|error| error.to_string())?;
        let outcome = pipeline
            .run_explicit_in(
                &format!("Run {tool}"),
                vec![step],
                Finish::Silent,
                Some(input),
                Some(&self.pack.source),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(outcome
            .state
            .results
            .get("run")
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub async fn migrate(&self, major: u64, root: &Path, apply: bool) -> Result<Value, String> {
        let plan = catalog_name(
            &self.pack.source.name,
            ItemKind::Plan,
            &format!("{MIGRATION_PREFIX}{major}"),
        );
        self.run(
            &plan,
            json!({"root": root.display().to_string(), "apply": apply}),
        )
        .await
    }

    pub fn usage(&self) -> Option<String> {
        let usage = self.runtime.usage.take();
        (!usage.is_empty()).then(|| usage.summary())
    }

    pub async fn shutdown(&self) {
        self.runtime.shutdown().await;
    }
}

fn is_identifier(key: &str) -> bool {
    key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn isolated_runtime(
    config: &Config,
    options: &RuntimeOptions,
    root: Option<&Path>,
    pack: LoadedPack,
) -> Result<Runtime> {
    let mut config = config.clone();
    config.tools.paths = root
        .map(|root| vec![root.join("tools")])
        .unwrap_or_default();
    config.plans.paths = root
        .map(|root| vec![root.join("plans")])
        .unwrap_or_default();
    Runtime::with_packs(config, InstalledPacks::of(pack), options.clone())
}

fn builtin_catalog(runtime: &Runtime) -> Result<ToolCatalog> {
    let mut catalog = runtime.tool_catalog(&runtime.plan_docs().docs)?;
    let builtin: BTreeSet<String> = runtime
        .agent_set()
        .iter()
        .filter(|doc| matches!(doc.source, AgentSource::Builtin))
        .map(|doc| doc.name)
        .collect();
    catalog.agents.retain(|name| builtin.contains(name));
    Ok(catalog)
}

fn agent_tool_problems(doc: &AgentDoc, catalog: &ToolCatalog, caller: &Origin) -> Vec<String> {
    let mut input = Map::new();
    input.insert("prompt".to_string(), json!(""));
    input.insert("tools".to_string(), json!(doc.tool_names()));
    let plan = vec![Step {
        id: "tools".to_string(),
        tool_name: AGENT_TOOL.to_string(),
        input,
        reasoning: None,
        files: None,
    }];
    resolve_step_tools(&plan, catalog)
        .into_iter()
        .chain(catalog.packs.plan_problems(caller, &plan))
        .map(|problem| {
            problem
                .strip_prefix("step tools: ")
                .unwrap_or(&problem)
                .to_string()
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

fn plan_label(doc: &PlanDoc) -> String {
    match &doc.path {
        Some(path) => path.display().to_string(),
        None => format!("plan '{}'", doc.identifier),
    }
}

fn agent_label(doc: &AgentDoc) -> String {
    match &doc.source {
        AgentSource::Pack { path, .. } | AgentSource::File(path) => path.display().to_string(),
        AgentSource::Builtin => format!("agent '{}'", doc.name),
    }
}

fn migration_problems(loaded: &LoadedPack) -> Vec<String> {
    let mut problems = Vec::new();
    for doc in loaded
        .plans
        .iter()
        .filter(|doc| loaded.is_migration(&doc.identifier))
    {
        let bare = split_catalog_name(&doc.identifier)
            .map(|(_, _, bare)| bare)
            .unwrap_or(&doc.identifier);
        let label = plan_label(doc);
        if migration_major(bare).is_none_or(|major| major < 2) {
            problems.push(format!(
                "{label}: migration plan '{bare}' must be named {MIGRATION_PREFIX}<N>, where N is the major version it migrates to (2 or more)"
            ));
        }
        let inputs: BTreeSet<&str> = doc
            .input_schema
            .as_ref()
            .and_then(|schema| schema.get("properties"))
            .and_then(Value::as_object)
            .map(|properties| properties.keys().map(String::as_str).collect())
            .unwrap_or_default();
        if inputs != BTreeSet::from(MIGRATION_INPUTS) {
            problems.push(format!(
                "{label}: migration plan '{bare}' must take exactly the inputs {{root, apply}}, got {{{}}}",
                inputs.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    problems
}

fn os_problem(dir: &Path) -> Option<String> {
    let manifest = load_manifest(&dir.join(MANIFEST_FILE)).ok()?;
    manifest.requires.os.is_empty().then(|| {
        format!("{MANIFEST_FILE}: requires.os is not set; list the systems this pack runs on, such as os = {SUPPORTED_OS:?}")
    })
}

pub async fn check(
    config: &Config,
    options: &RuntimeOptions,
    dir: &Path,
) -> Result<(LoadedPack, Report)> {
    if !dir.is_dir() {
        anyhow::bail!("{} is not a directory", dir.display());
    }
    let source = source_at(dir);
    let (loaded, mut problems) = inspect(source.clone());
    let mut notes = Vec::new();
    problems.extend(os_problem(dir));
    problems.extend(
        loaded
            .needs
            .unmet
            .iter()
            .map(|unmet| format!("this machine does not meet {unmet}")),
    );
    problems.extend(literal_self_names(dir, &source.name));
    problems.extend(migration_problems(&loaded));

    let runtime = isolated_runtime(config, options, None, loaded.clone())?;
    let mut catalog = builtin_catalog(&runtime)?;
    for (prefix, need) in &loaded.needs.mcp {
        if config.mcp.contains_key(prefix) {
            let server = runtime.registry.server_identity(prefix).await;
            problems.extend(requirements::mcp_problems(prefix, need, server));
        } else {
            catalog.mcp_servers.insert(prefix.clone());
            notes.push(format!(
                "requires.mcp.{prefix}: no [mcp.{prefix}] is configured here, so calls to {prefix}__* were not checked"
            ));
        }
    }
    runtime.shutdown().await;

    let origin = Origin::Pack(source.name.clone());
    for doc in &loaded.plans {
        let label = plan_label(doc);
        let found = resolve_plan_tools(doc, &catalog);
        problems.extend(found.errors.iter().map(|error| format!("{label}: {error}")));
        notes.extend(found.notes.iter().map(|note| format!("{label}: {note}")));
        if !loaded.is_migration(&doc.identifier) {
            for (step, tool) in step_tools(&doc.steps) {
                if loaded.is_migration(&tool) {
                    problems.push(format!(
                        "{label}: step {step}: '{tool}' is a migration item; only migration plans can call it"
                    ));
                }
            }
        }
    }
    let (set, _) = AgentSet::load(BUILTINS, &[]);
    let set = set.with_packs(loaded.agents.iter().cloned());
    let fragments: Vec<&str> = dive_core::agent::doc::global_fragments()
        .into_keys()
        .collect();
    for doc in &loaded.agents {
        let label = agent_label(doc);
        problems.extend(set.agent_problems(&doc.name, &fragments));
        problems.extend(
            agent_tool_problems(doc, &catalog, &origin)
                .into_iter()
                .map(|problem| format!("{label}: tools: {problem}")),
        );
        for name in &doc.tool_names() {
            if loaded.is_migration(name) {
                problems.push(format!(
                    "{label}: tools: '{name}' is a migration item; only migration plans can call it"
                ));
            }
        }
    }
    Ok((loaded, Report { problems, notes }))
}

pub fn migration_cases(dir: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir.join(MIGRATION_TESTS)) else {
        return Vec::new();
    };
    let mut cases: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let major = major_number(name.strip_prefix('v')?)?;
            Some((major, path))
        })
        .collect();
    cases.sort();
    cases
}

#[derive(Debug, Clone)]
pub struct CaseRun {
    pub case: String,
    pub ran: Vec<u64>,
    pub problems: Vec<String>,
}

pub async fn run_migration_case(
    session: &PackSession,
    config: &Config,
    majors: &[u64],
    major: u64,
    case: &Path,
) -> Result<CaseRun> {
    let label = format!("{MIGRATION_TESTS}/v{major}");
    let mut run = CaseRun {
        case: label.clone(),
        ran: Vec::new(),
        problems: Vec::new(),
    };
    if !majors.contains(&major) {
        run.problems.push(format!(
            "{label}: there is no plans/migrations/{MIGRATION_PREFIX}{major}.yaml to run over it"
        ));
        return Ok(run);
    }
    let scratch = tempfile::tempdir()?;
    let root = scratch.path().join("dive");
    copy_tree(case, &root)?;
    for step in majors.iter().copied().filter(|step| *step >= major) {
        if let Err(error) = session.migrate(step, &root, true).await {
            run.problems
                .push(format!("{label}: {MIGRATION_PREFIX}{step} failed: {error}"));
            return Ok(run);
        }
        run.ran.push(step);
    }
    let (current, _) = inspect_pack(session.pack.source.clone(), &[], &[]);
    run.problems.extend(
        validate_user_files(config, session.runtime.options(), current, &root)?
            .into_iter()
            .map(|problem| format!("{label}: {problem}")),
    );
    Ok(run)
}

pub fn validate_user_files(
    config: &Config,
    options: &RuntimeOptions,
    pack: LoadedPack,
    root: &Path,
) -> Result<Vec<String>> {
    let relative = |text: String| text.replace(&format!("{}/", root.display()), "");
    let mut problems = Vec::new();
    let runtime = isolated_runtime(config, options, Some(root), pack)?;
    if let Err(error) = runtime.tool_docs() {
        problems.push(relative(error.to_string()));
    }
    let plans = load_plan_docs(&[root.join("plans")]);
    problems.extend(
        plans
            .skipped
            .iter()
            .map(|error| relative(error.to_string())),
    );
    let agents = dive_core::agent::doc::load_agent_dir(&root.join("agents"));
    problems.extend(agents.errors.iter().cloned().map(relative));

    let mut catalog = builtin_catalog(&runtime)?;
    catalog
        .plans
        .extend(plans.docs.iter().map(|doc| doc.identifier.clone()));
    catalog
        .agents
        .extend(agents.docs.iter().map(|doc| doc.name.clone()));
    for doc in &plans.docs {
        let label = relative(plan_label(doc));
        problems.extend(
            resolve_plan_tools(doc, &catalog)
                .errors
                .into_iter()
                .map(|error| format!("{label}: {error}")),
        );
    }
    for doc in &agents.docs {
        let label = relative(agent_label(doc));
        problems.extend(
            agent_tool_problems(doc, &catalog, &Origin::Outside)
                .into_iter()
                .map(|problem| format!("{label}: tools: {problem}")),
        );
        for (field, names) in [("subagents", &doc.subagents), ("handoffs", &doc.handoffs)] {
            for name in names
                .iter()
                .filter(|name| catalog.packs.routed(name).is_some())
            {
                let problem = match catalog.packs.item(name) {
                    None => Some(format!("'{name}' is not an item of the pack")),
                    Some(_) => catalog.packs.refusal(&Origin::Outside, name),
                };
                if let Some(problem) = problem {
                    problems.push(format!("{label}: {field}: {problem}"));
                }
            }
        }
    }
    Ok(problems)
}

pub fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
