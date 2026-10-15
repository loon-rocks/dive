use super::requirements::{evaluate, Machine, McpNeed, McpUse, PackNeeds};
use super::{apply_interface, catalog_name, ItemKind, PackItem, PackSource};
use crate::agent::doc::{global_fragments, load_agent_file, AgentDoc, AgentSource};
use crate::pipeline::catalog::{agent_tool_patterns, step_calls, step_tools};
use crate::pipeline::doc::{parse_plan_doc, validate_doc, PlanDoc};
use crate::user_tools::{load_user_tool, UserToolDoc};
use graph_config::pack::{
    check_pack_name, load_manifest, PackManifest, MANIFEST_FILE, MAX_CATALOG_NAME, PACK_FORMAT,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const MIGRATIONS_DIR: &str = "migrations";

#[derive(Debug, Clone)]
pub struct LoadedPack {
    pub source: PackSource,
    pub manifest: PackManifest,
    pub tools: Vec<UserToolDoc>,
    pub plans: Vec<PlanDoc>,
    pub agents: Vec<AgentDoc>,
    pub items: Vec<PackItem>,
    pub needs: PackNeeds,
    pub migrations: Vec<String>,
}

pub fn check_installed_name(name: &str, mcp_servers: &BTreeSet<String>) -> Result<(), String> {
    check_pack_name(name)?;
    if mcp_servers.contains(name) {
        return Err(format!(
            "pack name '{name}' is also a configured MCP server ([mcp.{name}]); install the pack under another name with --as"
        ));
    }
    Ok(())
}

pub fn load_pack(
    source: PackSource,
    include: &[String],
    exclude: &[String],
) -> Result<LoadedPack, Vec<String>> {
    let (loaded, problems) = inspect_pack(source, include, exclude);
    if problems.is_empty() {
        return Ok(loaded);
    }
    let pack = &loaded.source.name;
    Err(problems
        .into_iter()
        .map(|problem| format!("pack '{pack}': {problem}"))
        .collect())
}

pub fn inspect_pack(
    source: PackSource,
    include: &[String],
    exclude: &[String],
) -> (LoadedPack, Vec<String>) {
    let pack = source.name.clone();
    let mut problems = Vec::new();
    if let Err(problem) = check_pack_name(&pack) {
        problems.push(problem);
    }
    let manifest = match load_manifest(&source.dir.join(MANIFEST_FILE)) {
        Ok(manifest) => manifest,
        Err(problem) => {
            problems.push(problem);
            PackManifest {
                version: PACK_FORMAT,
                name: pack.clone(),
                description: String::new(),
                license: None,
                homepage: None,
                requires: Default::default(),
            }
        }
    };

    let tools = load_dir(
        &source.dir.join("tools"),
        "tool name",
        load_user_tool,
        |doc: &UserToolDoc| &doc.name,
        &mut problems,
    );
    let plans = load_dir(
        &source.dir.join("plans"),
        "plan identifier",
        |path| load_plan(path, &source),
        |doc: &PlanDoc| &doc.identifier,
        &mut problems,
    );
    let agents = load_dir(
        &source.dir.join("agents"),
        "agent name",
        |path| load_agent(path, &source),
        |doc: &AgentDoc| &doc.name,
        &mut problems,
    );

    let mut items: Vec<PackItem> = tools
        .iter()
        .map(|doc| item(&pack, ItemKind::Tool, &doc.name))
        .chain(plans.iter().map(|doc| {
            item(
                &pack,
                ItemKind::Plan,
                bare(&pack, ItemKind::Plan, &doc.identifier),
            )
        }))
        .chain(agents.iter().map(|doc| {
            item(
                &pack,
                ItemKind::Agent,
                bare(&pack, ItemKind::Agent, &doc.name),
            )
        }))
        .collect();
    problems.extend(long_names(&items));

    let machine = Machine::current().with_stored(
        &manifest.requires.env,
        &crate::secrets::CredentialStore::system(),
    );
    let evaluation = evaluate(&manifest.requires, &machine);
    problems.extend(evaluation.refused);
    apply_interface(&mut items, include, exclude);
    let needs = PackNeeds {
        unmet: evaluation.unmet,
        mcp: manifest
            .requires
            .mcp
            .iter()
            .map(|(prefix, requirement)| {
                let need = McpNeed {
                    requirement: requirement.clone(),
                    uses: mcp_uses(prefix, &plans, &agents),
                };
                (prefix.clone(), need)
            })
            .collect(),
    };
    let loaded = LoadedPack {
        source,
        manifest,
        tools,
        plans,
        agents,
        items,
        needs,
        migrations: Vec::new(),
    };
    (loaded, problems)
}

impl LoadedPack {
    pub fn load_migrations(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        let source = self.source.clone();
        let tools = load_dir(
            &source.dir.join("tools").join(MIGRATIONS_DIR),
            "tool name",
            load_user_tool,
            |doc: &UserToolDoc| &doc.name,
            &mut problems,
        );
        let plans = load_dir(
            &source.dir.join("plans").join(MIGRATIONS_DIR),
            "plan identifier",
            |path| load_plan(path, &source),
            |doc: &PlanDoc| &doc.identifier,
            &mut problems,
        );
        let pack = source.name.clone();
        let mut added = Vec::new();
        for doc in tools {
            if self.tools.iter().any(|known| known.name == doc.name) {
                problems.push(format!(
                    "migration tool '{}' has the same name as a tool in tools/",
                    doc.name
                ));
                continue;
            }
            added.push(item(&pack, ItemKind::Tool, &doc.name));
            self.tools.push(doc);
        }
        for doc in plans {
            let name = bare(&pack, ItemKind::Plan, &doc.identifier).to_string();
            if self
                .plans
                .iter()
                .any(|known| known.identifier == doc.identifier)
            {
                problems.push(format!(
                    "migration plan '{name}' has the same identifier as a plan in plans/"
                ));
                continue;
            }
            added.push(item(&pack, ItemKind::Plan, &name));
            self.plans.push(doc);
        }
        problems.extend(long_names(&added));
        for mut found in added {
            found.public = false;
            self.migrations.push(found.catalog_name());
            self.items.push(found);
        }
        problems
    }

    pub fn is_migration(&self, name: &str) -> bool {
        self.migrations.iter().any(|known| known == name)
    }
}

fn bare<'a>(pack: &str, kind: ItemKind, name: &'a str) -> &'a str {
    name.strip_prefix(&catalog_name(pack, kind, ""))
        .unwrap_or(name)
}

fn long_names(items: &[PackItem]) -> Option<String> {
    let long: Vec<String> = items
        .iter()
        .map(PackItem::catalog_name)
        .filter(|name| name.len() > MAX_CATALOG_NAME)
        .collect();
    if long.is_empty() {
        return None;
    }
    Some(format!(
        "{} longer than {MAX_CATALOG_NAME} characters: {}; install the pack under a shorter name with --as, or shorten the item names",
        if long.len() == 1 { "a catalog name is" } else { "catalog names are" },
        long.iter()
            .map(|name| format!("{name} ({})", name.len()))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

pub fn literal_self_names(dir: &Path, pack: &str) -> Vec<String> {
    let own = |name: &str| {
        name.strip_prefix(pack)
            .and_then(|rest| rest.strip_prefix(super::KIND_SEPARATOR))
            .filter(|rest| !rest.is_empty())
            .map(|rest| {
                format!(
                    "names its own pack literally as '{name}'; write '{}{rest}' so it resolves under any installed name",
                    super::SELF_PREFIX
                )
            })
    };
    let mut problems = Vec::new();
    let plan_dirs = [dir.join("plans"), dir.join("plans").join(MIGRATIONS_DIR)];
    for path in plan_dirs.iter().flat_map(|dir| yaml_files(dir)) {
        let Ok(doc) = parse_plan_doc(&path) else {
            continue;
        };
        let names = step_tools(&doc.steps)
            .into_iter()
            .chain(agent_tool_patterns(&doc.steps));
        for (step, name) in names {
            if let Some(problem) = own(&name) {
                problems.push(format!("{}: step {step}: {problem}", path.display()));
            }
        }
    }
    for path in yaml_files(&dir.join("agents")) {
        let Ok(doc) = load_agent_file(&path) else {
            continue;
        };
        for (field, names) in [
            ("tools", &doc.tools),
            ("subagents", &doc.subagents),
            ("handoffs", &doc.handoffs),
        ] {
            for problem in names.iter().filter_map(|name| own(name)) {
                problems.push(format!("{}: {field}: {problem}", path.display()));
            }
        }
    }
    problems
}

fn mcp_uses(prefix: &str, plans: &[PlanDoc], agents: &[AgentDoc]) -> Vec<McpUse> {
    let server_tool = |name: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix(super::KIND_SEPARATOR))
            .is_some_and(|tool| !tool.is_empty() && !name.contains('*'))
    };
    let mut uses: Vec<McpUse> = Vec::new();
    let mut add = |found: McpUse| {
        let known = uses
            .iter()
            .any(|seen| seen.tool == found.tool && seen.input == found.input);
        if !known {
            uses.push(found);
        }
    };
    for plan in plans {
        for (step, tool, input) in step_calls(&plan.steps) {
            if server_tool(&tool) {
                add(McpUse {
                    site: format!("plan '{}' step {step}", plan.identifier),
                    tool,
                    input: Some(input),
                });
            }
        }
        for (step, pattern) in agent_tool_patterns(&plan.steps) {
            if server_tool(&pattern) {
                add(McpUse {
                    site: format!("plan '{}' step {step} tools", plan.identifier),
                    tool: pattern,
                    input: None,
                });
            }
        }
    }
    for agent in agents {
        for tool in agent.tools.iter().filter(|tool| server_tool(tool)) {
            add(McpUse {
                site: format!("agent '{}' tools", agent.name),
                tool: tool.clone(),
                input: None,
            });
        }
    }
    uses
}

fn load_dir<T>(
    dir: &Path,
    noun: &str,
    load: impl Fn(&Path) -> Result<T, String>,
    key: impl Fn(&T) -> &String,
    problems: &mut Vec<String>,
) -> Vec<T> {
    let mut docs: Vec<T> = Vec::new();
    for path in yaml_files(dir) {
        match load(&path) {
            Ok(doc) if docs.iter().any(|known| key(known) == key(&doc)) => problems.push(format!(
                "{}: duplicate {noun} '{}'",
                path.display(),
                key(&doc)
            )),
            Ok(doc) => docs.push(doc),
            Err(problem) => problems.push(problem),
        }
    }
    docs
}

fn item(pack: &str, kind: ItemKind, name: &str) -> PackItem {
    PackItem {
        pack: pack.to_string(),
        kind,
        name: name.to_string(),
        public: true,
    }
}

fn load_plan(path: &Path, source: &PackSource) -> Result<PlanDoc, String> {
    let mut doc = parse_plan_doc(path).map_err(|e| e.to_string())?;
    let problems = super::resolve_plan_steps(&mut doc.steps, &source.name);
    if !problems.is_empty() {
        return Err(format!("{}: {}", path.display(), problems.join("; ")));
    }
    doc.identifier = catalog_name(&source.name, ItemKind::Plan, &doc.identifier);
    doc.pack = Some(source.clone());
    validate_doc(&doc).map_err(|message| format!("{}: {message}", path.display()))?;
    Ok(doc)
}

fn load_agent(path: &Path, source: &PackSource) -> Result<AgentDoc, String> {
    let mut doc = load_agent_file(path)?;
    let mut problems = super::resolve_agent(&mut doc, &source.name);
    doc.name = catalog_name(&source.name, ItemKind::Agent, &doc.name);
    doc.source = AgentSource::Pack {
        source: source.clone(),
        path: path.to_path_buf(),
    };
    if problems.is_empty() {
        let fragments: Vec<&str> = global_fragments().into_keys().collect();
        problems = crate::agent::doc::validate_doc(&doc, &fragments);
    }
    if problems.is_empty() {
        Ok(doc)
    } else {
        Err(format!("{}: {}", path.display(), problems.join("; ")))
    }
}

fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yaml") | Some("yml")
            )
        })
        .collect();
    paths.sort();
    paths
}
