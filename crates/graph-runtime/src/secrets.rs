use crate::packs::InstalledPacks;
use crate::runtime::Workspace;
use anyhow::{bail, Result};
use graph_core::secrets::references::{self, Reference};
use graph_core::secrets::{
    is_secret_name, ChainedSource, CredentialStore, EnvSource, SecretSource, SecretStore,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct Secrets {
    pub env: Arc<dyn SecretSource>,
    pub store: Arc<dyn SecretStore>,
}

impl Secrets {
    pub fn system() -> Self {
        Self::new(Arc::new(EnvSource), Arc::new(CredentialStore::system()))
    }

    pub fn new(env: Arc<dyn SecretSource>, store: Arc<dyn SecretStore>) -> Self {
        Self { env, store }
    }

    pub fn resolver(&self) -> ChainedSource {
        let store: Arc<dyn SecretSource> = self.store.clone();
        ChainedSource::new(vec![self.env.clone(), store])
    }

    fn env_value(&self, name: &str) -> Option<String> {
        self.env
            .resolve(name)
            .map(|found| found.value)
            .filter(|value| !value.is_empty())
    }

    fn store_value(&self, name: &str, project: Option<&str>) -> Option<String> {
        self.store
            .get_in(name, project)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
    }

    fn read_error(&self, name: &str, project: Option<&str>) -> Option<String> {
        if self.store.status().is_err() {
            return None;
        }
        let layers = project.map(Some).into_iter().chain(std::iter::once(None));
        layers
            .filter_map(|layer| self.store.get_in(name, layer).err())
            .map(|error| error.to_string())
            .next()
    }

    fn values(&self, name: &str, project: Option<&str>) -> Values {
        Values {
            env: self.env_value(name),
            project: project.and_then(|id| self.store_value(name, Some(id))),
            shared: self.store_value(name, None),
        }
    }

    fn store_state(&self) -> StoreState {
        match self.store.status() {
            Ok(()) => StoreState {
                available: true,
                reason: None,
            },
            Err(error) => StoreState {
                available: false,
                reason: Some(error.to_string()),
            },
        }
    }
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("env", &self.env)
            .field("store", &self.store)
            .finish()
    }
}

struct Values {
    env: Option<String>,
    project: Option<String>,
    shared: Option<String>,
}

impl Values {
    fn in_use(&self) -> Option<Layer> {
        if self.env.is_some() {
            Some(Layer::Env)
        } else if self.project.is_some() {
            Some(Layer::Project)
        } else if self.shared.is_some() {
            Some(Layer::Shared)
        } else {
            None
        }
    }

    fn value(&self, layer: Layer) -> Option<&String> {
        match layer {
            Layer::Env => self.env.as_ref(),
            Layer::Project => self.project.as_ref(),
            Layer::Shared => self.shared.as_ref(),
        }
    }

    fn status(&self) -> SecretStatus {
        let stored = self.project.is_some() || self.shared.is_some();
        match (self.env.is_some(), stored) {
            (true, true) => SecretStatus::EnvShadowsStore,
            (true, false) => SecretStatus::Env,
            (false, true) => SecretStatus::Store,
            (false, false) => SecretStatus::Missing,
        }
    }

    fn masked(&self, layer: Layer) -> Option<String> {
        self.value(layer).map(|value| mask(value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretScope {
    Project,
    Shared,
}

impl SecretScope {
    pub fn layer(self) -> Layer {
        match self {
            SecretScope::Project => Layer::Project,
            SecretScope::Shared => Layer::Shared,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Env,
    Project,
    Shared,
}

impl Layer {
    pub const ALL: [Layer; 3] = [Layer::Env, Layer::Project, Layer::Shared];

    pub fn label(self) -> &'static str {
        match self {
            Layer::Env => "env",
            Layer::Project => "project",
            Layer::Shared => "shared",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretStatus {
    Env,
    Store,
    EnvShadowsStore,
    Missing,
}

impl SecretStatus {
    pub fn label(self) -> &'static str {
        match self {
            SecretStatus::Env => "env",
            SecretStatus::Store => "store",
            SecretStatus::EnvShadowsStore => "env (shadows store)",
            SecretStatus::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreState {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretEntry {
    pub name: String,
    pub status: SecretStatus,
    pub in_use: Option<Layer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub references: Vec<Reference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretList {
    pub secrets: Vec<SecretEntry>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub store: StoreState,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretDetail {
    pub name: String,
    pub status: SecretStatus,
    pub in_use: Option<Layer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub references: Vec<Reference>,
    pub store: StoreState,
    pub problems: Vec<String>,
}

impl SecretDetail {
    pub fn value(&self, layer: Layer) -> Option<&String> {
        match layer {
            Layer::Env => self.env_value.as_ref(),
            Layer::Project => self.project_value.as_ref(),
            Layer::Shared => self.shared_value.as_ref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretSaved {
    pub name: String,
    pub scope: SecretScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub shadowed_by: Option<Layer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretRemoved {
    pub name: String,
    pub scope: SecretScope,
    pub removed: bool,
}

pub fn description(references: &[Reference]) -> Option<String> {
    references
        .iter()
        .find_map(|reference| reference.description.clone())
}

pub fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 8 {
        return "****".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("****{tail}")
}

fn check_name(name: &str) -> Result<()> {
    if !is_secret_name(name) {
        bail!("{name} is not a variable name: use letters, digits and underscores, not starting with a digit");
    }
    Ok(())
}

pub fn project_id(workspace: &Workspace) -> Option<String> {
    graph_config::project_id(&workspace.config_paths())
}

fn project_config(workspace: &Workspace) -> PathBuf {
    workspace.project_dir().join("config.toml")
}

struct Collected {
    references: BTreeMap<String, Vec<Reference>>,
    problems: Vec<String>,
    project: Option<String>,
}

fn resolve_dir(root: &Path, path: &Path) -> PathBuf {
    let path = graph_config::expand_tilde(path);
    if path.is_relative() && root != Path::new(".") {
        root.join(path)
    } else {
        path
    }
}

fn collect(workspace: &Workspace, secrets: &Secrets) -> Collected {
    collect_layers(&workspace.config_paths(), workspace.root(), secrets)
}
fn collect_layers(paths: &[PathBuf], root: &Path, secrets: &Secrets) -> Collected {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    for path in paths {
        let path = graph_config::expand_tilde(path);
        if !path.exists() {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match references::in_config(&path, &text) {
                Ok(references) => found.extend(references),
                Err(problem) => problems.push(problem),
            },
            Err(error) => problems.push(format!("reading {}: {error}", path.display())),
        }
    }
    match graph_config::load_from(paths, &secrets.resolver()) {
        Err(error) => problems.push(format!("{error:#}")),
        Ok(loaded) => {
            let dirs: Vec<PathBuf> = loaded
                .config
                .tools
                .paths
                .iter()
                .map(|path| resolve_dir(root, path))
                .collect();
            let mut tool_dirs = graph_core::user_tools::builtin_override_dirs(&dirs);
            tool_dirs.extend(dirs);
            for dir in tool_dirs {
                match graph_core::user_tools::load_user_tools(std::slice::from_ref(&dir)) {
                    Ok(docs) => {
                        for doc in &docs {
                            found.extend(references::in_tool(doc, None));
                        }
                    }
                    Err(problem) => problems.push(problem),
                }
            }
            match InstalledPacks::for_config(&loaded) {
                Err(error) => problems.push(format!("{error:#}")),
                Ok(installed) => {
                    for pack in &installed.packs {
                        let name = pack.name();
                        for doc in &pack.loaded.tools {
                            found.extend(references::in_tool(doc, Some(name)));
                        }
                        found.extend(references::in_requires(
                            name,
                            &pack
                                .loaded
                                .source
                                .dir
                                .join(graph_config::pack::MANIFEST_FILE),
                            &pack.loaded.manifest.requires.env,
                        ));
                    }
                }
            }
        }
    }
    let mut grouped: BTreeMap<String, Vec<Reference>> = BTreeMap::new();
    for reference in found {
        let entries = grouped.entry(reference.name.clone()).or_default();
        if !entries.contains(&reference) {
            entries.push(reference);
        }
    }
    Collected {
        references: grouped,
        problems,
        project: graph_config::project_id(paths),
    }
}

pub fn list(workspace: &Workspace, secrets: &Secrets) -> SecretList {
    listed(collect(workspace, secrets), secrets)
}

fn stored_names(secrets: &Secrets, project: Option<&str>) -> Vec<String> {
    secrets
        .store
        .accounts()
        .unwrap_or_default()
        .iter()
        .filter_map(|account| graph_core::secrets::split_account(account))
        .filter(|(_, scope)| scope.is_none() || *scope == project)
        .map(|(name, _)| name.to_string())
        .collect()
}

fn listed(mut collected: Collected, secrets: &Secrets) -> SecretList {
    for name in stored_names(secrets, collected.project.as_deref()) {
        collected.references.entry(name).or_default();
    }
    let project = collected.project.as_deref();
    let entries: Vec<SecretEntry> = collected
        .references
        .into_iter()
        .map(|(name, references)| {
            let values = secrets.values(&name, project);
            SecretEntry {
                status: values.status(),
                in_use: values.in_use(),
                read_error: secrets.read_error(&name, project),
                env_value: values.masked(Layer::Env),
                project_value: values.masked(Layer::Project),
                shared_value: values.masked(Layer::Shared),
                description: description(&references),
                name,
                references,
            }
        })
        .collect();
    SecretList {
        count: entries.len(),
        secrets: entries,
        project: collected.project.clone(),
        store: secrets.store_state(),
        problems: collected.problems,
    }
}

pub fn show(workspace: &Workspace, secrets: &Secrets, name: &str) -> Result<SecretDetail> {
    check_name(name)?;
    Ok(detail(collect(workspace, secrets), secrets, name))
}

fn detail(mut collected: Collected, secrets: &Secrets, name: &str) -> SecretDetail {
    let values = secrets.values(name, collected.project.as_deref());
    let references = collected.references.remove(name).unwrap_or_default();
    SecretDetail {
        name: name.to_string(),
        status: values.status(),
        in_use: values.in_use(),
        read_error: secrets.read_error(name, collected.project.as_deref()),
        env_value: values.masked(Layer::Env),
        project_value: values.masked(Layer::Project),
        shared_value: values.masked(Layer::Shared),
        project: collected.project.take(),
        description: description(&references),
        references,
        store: secrets.store_state(),
        problems: collected.problems,
    }
}

pub fn reveal(secrets: &Secrets, workspace: &Workspace, name: &str) -> Option<String> {
    let values = secrets.values(name, project_id(workspace).as_deref());
    let layer = values.in_use()?;
    values.value(layer).cloned()
}

pub fn set(
    secrets: &Secrets,
    workspace: &Workspace,
    name: &str,
    value: &str,
    scope: SecretScope,
) -> Result<SecretSaved> {
    check_name(name)?;
    if value.is_empty() {
        bail!("no value given for {name}; an empty value is never stored");
    }
    secrets.store.status()?;
    let project = match scope {
        SecretScope::Project => Some(graph_config::ensure_project_id(&project_config(workspace))?),
        SecretScope::Shared => project_id(workspace),
    };
    let target = match scope {
        SecretScope::Project => project.as_deref(),
        SecretScope::Shared => None,
    };
    secrets.store.set_in(name, target, value)?;
    let values = secrets.values(name, project.as_deref());
    let shadowed_by = values.in_use().filter(|layer| *layer < scope.layer());
    Ok(SecretSaved {
        name: name.to_string(),
        scope,
        project: target.map(str::to_string),
        shadowed_by,
    })
}

pub fn remove(
    secrets: &Secrets,
    workspace: &Workspace,
    name: &str,
    scope: SecretScope,
) -> Result<SecretRemoved> {
    check_name(name)?;
    let removed = match scope {
        SecretScope::Shared => secrets.store.remove_in(name, None)?,
        SecretScope::Project => match project_id(workspace) {
            Some(id) => secrets.store.remove_in(name, Some(&id))?,
            None => false,
        },
    };
    Ok(SecretRemoved {
        name: name.to_string(),
        scope,
        removed,
    })
}

#[cfg(test)]
mod tests;
