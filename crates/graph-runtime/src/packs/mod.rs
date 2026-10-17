pub mod author;
pub mod git;
pub mod hash;
pub mod layer;
pub mod refs;
pub mod release;
pub mod scaffold;

pub use hash::content_hash;

use graph_config::pack::{load_lock, LayerKind, LockEntry, PackEntry, PackLayer};
use graph_core::agent::doc::AgentDoc;
use graph_core::packs::{
    check_installed_name, load_pack, requirements, LoadedPack, PackSet, PackSource,
};
use graph_core::pipeline::doc::PlanDoc;
use graph_core::user_tools::UserToolDoc;
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum PackCheck {
    Installed(InstalledCheck),
    Shadowed(ShadowedCheck),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledCheck {
    pub name: String,
    pub layer: String,
    pub dir: String,
    pub git: String,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub path: String,
    pub tag: Option<String>,
    pub sha: String,
    pub hash: String,
    pub locked_hash: String,
    pub modified: bool,
    pub unmet: Vec<String>,
    pub shadowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowedCheck {
    pub name: String,
    pub layer: String,
    pub dir: String,
    pub git: String,
    pub path: String,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub shadowed: bool,
    pub shadowed_by: String,
}

#[derive(Debug, Clone)]
pub struct InstalledPack {
    pub loaded: LoadedPack,
    pub entry: PackEntry,
    pub lock: LockEntry,
    pub hash: String,
}

impl InstalledPack {
    pub fn name(&self) -> &str {
        &self.loaded.source.name
    }

    pub fn modified(&self) -> bool {
        self.hash != self.lock.hash
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShadowedPack {
    pub name: String,
    pub entry: PackEntry,
    pub layer: LayerKind,
    pub dir: PathBuf,
    pub by: LayerKind,
}

#[derive(Debug, Clone, Default)]
pub struct InstalledPacks {
    pub packs: Vec<InstalledPack>,
    pub shadowed: Vec<ShadowedPack>,
    pub warnings: Vec<String>,
}

fn add_command(name: &str, layer: LayerKind) -> String {
    match layer {
        LayerKind::Global => format!("graph pack add {name} --global"),
        LayerKind::Project => format!("graph pack add {name}"),
    }
}

impl InstalledPacks {
    pub fn for_config(loaded: &graph_config::LoadedConfig) -> anyhow::Result<Self> {
        Self::load(
            &loaded.pack_layers,
            &loaded.config.mcp.keys().cloned().collect(),
        )
    }

    pub fn load(layers: &[PackLayer], mcp_servers: &BTreeSet<String>) -> anyhow::Result<Self> {
        let mut installed = InstalledPacks::default();
        let mut problems = Vec::new();
        for (index, layer) in layers.iter().enumerate() {
            let config = layer.dir.join("config.toml");
            let lock_path = layer.lock_path();
            let packs_dir = layer.packs_dir();
            let lock = match load_lock(&lock_path) {
                Ok(lock) => lock,
                Err(problem) => {
                    problems.push(problem);
                    continue;
                }
            };
            for copy in copies(&packs_dir) {
                if !layer.packs.contains_key(&copy) {
                    problems.push(format!(
                        "{} is not a configured pack: add [packs.{copy}] to {} or delete the directory",
                        packs_dir.join(&copy).display(),
                        config.display()
                    ));
                }
            }
            for (name, entry) in &layer.packs {
                let dir = packs_dir.join(name);
                if let Some(by) = layers[index + 1..].iter().rev().find(|later| {
                    later.packs.contains_key(name) && later.packs_dir().join(name).is_dir()
                }) {
                    installed.shadowed.push(ShadowedPack {
                        name: name.clone(),
                        entry: entry.clone(),
                        layer: layer.kind,
                        dir,
                        by: by.kind,
                    });
                    continue;
                }
                if let Err(problem) = check_installed_name(name, mcp_servers) {
                    problems.push(format!("{}: {problem}", config.display()));
                    continue;
                }
                if !dir.is_dir() {
                    installed.warnings.push(format!(
                        "pack '{name}' is configured in {} but not installed at {}, so its tools, plans and agents are unavailable; run `{}`",
                        config.display(),
                        dir.display(),
                        add_command(name, layer.kind)
                    ));
                    continue;
                }
                let Some(locked) = lock.packs.get(name) else {
                    problems.push(format!(
                        "pack '{name}' at {} has no entry in {}; reinstall it with `{}`",
                        dir.display(),
                        lock_path.display(),
                        add_command(name, layer.kind)
                    ));
                    continue;
                };
                let hash = match content_hash(&dir) {
                    Ok(hash) => hash,
                    Err(error) => {
                        problems.push(format!("pack '{name}': reading {}: {error}", dir.display()));
                        continue;
                    }
                };
                if hash != locked.hash {
                    installed.warnings.push(format!(
                        "pack '{name}' at {} has local edits: its content no longer matches {} ({hash}, locked {})",
                        dir.display(),
                        lock_path.display(),
                        locked.hash
                    ));
                }
                let source = PackSource {
                    name: name.clone(),
                    tag: refs::label(&locked.path, locked.tag.as_deref()),
                    layer: layer.kind,
                    dir: dir.clone(),
                };
                match load_pack(source, &entry.include, &entry.exclude) {
                    Ok(loaded) => installed.packs.push(InstalledPack {
                        loaded,
                        entry: entry.clone(),
                        lock: locked.clone(),
                        hash,
                    }),
                    Err(errors) => problems.extend(errors),
                }
            }
        }
        if !problems.is_empty() {
            anyhow::bail!("packs failed to load:\n  - {}", problems.join("\n  - "));
        }
        Ok(installed)
    }

    pub fn of(loaded: LoadedPack) -> Self {
        let git = loaded.source.dir.display().to_string();
        InstalledPacks {
            packs: vec![InstalledPack {
                entry: PackEntry {
                    git: git.clone(),
                    path: String::new(),
                    reference: None,
                    include: Vec::new(),
                    exclude: Vec::new(),
                },
                lock: LockEntry {
                    git,
                    path: String::new(),
                    tag: loaded.source.tag.clone(),
                    sha: String::new(),
                    hash: String::new(),
                },
                hash: String::new(),
                loaded,
            }],
            ..Default::default()
        }
    }

    pub fn pack_set(&self) -> PackSet {
        let mut set = PackSet::default();
        for pack in &self.packs {
            set.add(&pack.loaded);
        }
        set
    }

    pub fn tool_docs(&self) -> Vec<(PackSource, Vec<UserToolDoc>)> {
        self.packs
            .iter()
            .map(|pack| (pack.loaded.source.clone(), pack.loaded.tools.clone()))
            .collect()
    }

    pub fn plans(&self) -> impl Iterator<Item = &PlanDoc> {
        self.packs.iter().flat_map(|pack| pack.loaded.plans.iter())
    }

    pub fn agents(&self) -> impl Iterator<Item = &AgentDoc> {
        self.packs.iter().flat_map(|pack| pack.loaded.agents.iter())
    }

    pub fn public_agents(&self) -> Vec<AgentDoc> {
        let set = self.pack_set();
        self.agents()
            .filter(|doc| set.shows(&graph_core::packs::Origin::Outside, &doc.name))
            .cloned()
            .collect()
    }

    pub async fn unmet(&self, registry: &dyn graph_core::ToolRegistry) -> requirements::Unmet {
        requirements::unmet(
            &self.pack_set(),
            self.packs.iter().map(InstalledPack::name),
            registry,
        )
        .await
    }

    pub fn check_text(&self, unmet: &requirements::Unmet) -> String {
        let mut text = String::new();
        for pack in &self.packs {
            text.push_str(&format!(
                "{}\t{}\t{}\n",
                pack.loaded.source.describe(),
                pack.loaded.source.dir.display(),
                if pack.modified() {
                    "local edits"
                } else {
                    "matches packs.lock"
                }
            ));
            for problem in unmet.get(pack.name()).into_iter().flatten() {
                text.push_str(&format!("  unmet {problem}\n"));
            }
        }
        for shadowed in &self.shadowed {
            text.push_str(&format!(
                "pack {} ({})\t{}\tshadowed by the {} layer\n",
                shadowed.name,
                graph_core::packs::layer_label(shadowed.layer),
                shadowed.dir.display(),
                graph_core::packs::layer_label(shadowed.by)
            ));
        }
        text
    }

    pub fn check_entries(&self, unmet: &requirements::Unmet) -> Vec<PackCheck> {
        let mut entries: Vec<PackCheck> = self
            .packs
            .iter()
            .map(|pack| {
                PackCheck::Installed(InstalledCheck {
                    name: pack.name().to_string(),
                    layer: graph_core::packs::layer_label(pack.loaded.source.layer).to_string(),
                    dir: pack.loaded.source.dir.display().to_string(),
                    git: pack.entry.git.clone(),
                    reference: pack.entry.reference.clone(),
                    path: pack.entry.path.clone(),
                    tag: pack.lock.tag.clone(),
                    sha: pack.lock.sha.clone(),
                    hash: pack.hash.clone(),
                    locked_hash: pack.lock.hash.clone(),
                    modified: pack.modified(),
                    unmet: unmet.get(pack.name()).cloned().unwrap_or_default(),
                    shadowed: false,
                })
            })
            .collect();
        entries.extend(self.shadowed.iter().map(|shadowed| {
            PackCheck::Shadowed(ShadowedCheck {
                name: shadowed.name.clone(),
                layer: graph_core::packs::layer_label(shadowed.layer).to_string(),
                dir: shadowed.dir.display().to_string(),
                git: shadowed.entry.git.clone(),
                path: shadowed.entry.path.clone(),
                reference: shadowed.entry.reference.clone(),
                shadowed: true,
                shadowed_by: graph_core::packs::layer_label(shadowed.by).to_string(),
            })
        }));
        entries
    }

    pub fn warn(&self) {
        for warning in &self.warnings {
            tracing::warn!("{warning}");
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequirementReport {
    pub unmet: Vec<String>,
    pub snippets: Vec<String>,
}

impl RequirementReport {
    pub fn of(loaded: &LoadedPack, mcp_servers: &BTreeSet<String>) -> Self {
        let mut report = RequirementReport {
            unmet: loaded.needs.unmet.clone(),
            snippets: Vec::new(),
        };
        for (prefix, need) in &loaded.needs.mcp {
            if mcp_servers.contains(prefix) {
                continue;
            }
            report
                .unmet
                .extend(requirements::mcp_problems(prefix, need, None));
            report
                .snippets
                .extend(requirements::mcp_snippet(prefix, &need.requirement));
        }
        report
    }

    pub fn lines(&self, name: &str) -> Vec<String> {
        let mut lines: Vec<String> = self
            .unmet
            .iter()
            .map(|problem| format!("{name}: unmet {problem}"))
            .collect();
        if !self.snippets.is_empty() {
            lines.push(format!(
                "{name} needs MCP servers graph does not write for you; add to your config:"
            ));
            lines.extend(
                self.snippets
                    .iter()
                    .flat_map(|snippet| snippet.lines())
                    .map(|line| format!("  {line}")),
            );
        }
        lines
    }
}

fn copies(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests;
