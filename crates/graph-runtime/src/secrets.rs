use crate::packs::InstalledPacks;
use crate::runtime::Workspace;
use anyhow::{bail, Result};
use graph_core::secrets::references::{self, Reference};
use graph_core::secrets::{
    is_secret_name, ChainedSource, CredentialStore, EnvSource, SecretSource, SecretStore,
};
use serde::Serialize;
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

    fn store_value(&self, name: &str) -> Option<String> {
        self.store
            .get(name)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
    }

    fn status(&self, name: &str) -> SecretStatus {
        match (self.env_value(name), self.store_value(name)) {
            (Some(_), Some(_)) => SecretStatus::EnvShadowsStore,
            (Some(_), None) => SecretStatus::Env,
            (None, Some(_)) => SecretStatus::Store,
            (None, None) => SecretStatus::Missing,
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
pub struct SecretEntry {
    pub name: String,
    pub status: SecretStatus,
    pub references: Vec<Reference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretList {
    pub secrets: Vec<SecretEntry>,
    pub count: usize,
    pub store: StoreState,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretDetail {
    pub name: String,
    pub status: SecretStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_value: Option<String>,
    pub references: Vec<Reference>,
    pub store: StoreState,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretSaved {
    pub name: String,
    pub shadowed_by_env: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretRemoved {
    pub name: String,
    pub removed: bool,
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

struct Collected {
    references: BTreeMap<String, Vec<Reference>>,
    problems: Vec<String>,
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
    }
}

pub fn list(workspace: &Workspace, secrets: &Secrets) -> SecretList {
    listed(collect(workspace, secrets), secrets)
}

fn listed(collected: Collected, secrets: &Secrets) -> SecretList {
    let entries: Vec<SecretEntry> = collected
        .references
        .into_iter()
        .map(|(name, references)| SecretEntry {
            status: secrets.status(&name),
            name,
            references,
        })
        .collect();
    SecretList {
        count: entries.len(),
        secrets: entries,
        store: secrets.store_state(),
        problems: collected.problems,
    }
}

pub fn show(workspace: &Workspace, secrets: &Secrets, name: &str) -> Result<SecretDetail> {
    check_name(name)?;
    Ok(detail(collect(workspace, secrets), secrets, name))
}

fn detail(mut collected: Collected, secrets: &Secrets, name: &str) -> SecretDetail {
    SecretDetail {
        name: name.to_string(),
        status: secrets.status(name),
        env_value: secrets.env_value(name).map(|value| mask(&value)),
        store_value: secrets.store_value(name).map(|value| mask(&value)),
        references: collected.references.remove(name).unwrap_or_default(),
        store: secrets.store_state(),
        problems: collected.problems,
    }
}

pub fn set(secrets: &Secrets, name: &str, value: &str) -> Result<SecretSaved> {
    check_name(name)?;
    if value.is_empty() {
        bail!("no value given for {name}; an empty value is never stored");
    }
    secrets.store.set(name, value)?;
    Ok(SecretSaved {
        name: name.to_string(),
        shadowed_by_env: secrets.env_value(name).is_some(),
    })
}

pub fn remove(secrets: &Secrets, name: &str) -> Result<SecretRemoved> {
    check_name(name)?;
    Ok(SecretRemoved {
        name: name.to_string(),
        removed: secrets.store.remove(name)?,
    })
}

#[cfg(test)]
mod tests;
