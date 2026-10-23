mod memory;
pub mod references;
mod store;

pub use dive_config::secrets::{
    is_project_id, is_secret_name, EnvSource, InProject, Origin, Resolved, SecretSource,
};
pub use memory::MemorySecrets;
pub use store::{CredentialStore, SERVICE, STORE_ENV};

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("the OS credential store is unavailable: {0}")]
    Unavailable(String),
    #[error("{0} is not a variable name: use letters, digits and underscores, not starting with a digit")]
    InvalidName(String),
    #[error("{0} is not a project id: use letters, digits and hyphens")]
    InvalidProject(String),
    #[error("the OS credential store failed: {0}")]
    Failed(String),
}

pub trait SecretStore: SecretSource {
    fn status(&self) -> Result<(), StoreError>;
    fn get_in(&self, name: &str, project: Option<&str>) -> Result<Option<String>, StoreError>;
    fn set_in(&self, name: &str, project: Option<&str>, value: &str) -> Result<(), StoreError>;
    fn remove_in(&self, name: &str, project: Option<&str>) -> Result<bool, StoreError>;

    fn accounts(&self) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

    fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        self.get_in(name, None)
    }

    fn set(&self, name: &str, value: &str) -> Result<(), StoreError> {
        self.set_in(name, None, value)
    }

    fn remove(&self, name: &str) -> Result<bool, StoreError> {
        self.remove_in(name, None)
    }
}

impl<T: SecretStore + ?Sized> SecretStore for Arc<T> {
    fn status(&self) -> Result<(), StoreError> {
        (**self).status()
    }

    fn get_in(&self, name: &str, project: Option<&str>) -> Result<Option<String>, StoreError> {
        (**self).get_in(name, project)
    }

    fn set_in(&self, name: &str, project: Option<&str>, value: &str) -> Result<(), StoreError> {
        (**self).set_in(name, project, value)
    }

    fn remove_in(&self, name: &str, project: Option<&str>) -> Result<bool, StoreError> {
        (**self).remove_in(name, project)
    }

    fn accounts(&self) -> Result<Vec<String>, StoreError> {
        (**self).accounts()
    }
}

pub fn split_account(account: &str) -> Option<(&str, Option<&str>)> {
    let (name, project) = match account.split_once('@') {
        Some((name, project)) => (name, Some(project)),
        None => (account, None),
    };
    (is_secret_name(name) && project.is_none_or(is_project_id)).then_some((name, project))
}

pub fn store_account(name: &str, project: Option<&str>) -> Result<String, StoreError> {
    if !is_secret_name(name) {
        return Err(StoreError::InvalidName(name.to_string()));
    }
    if let Some(id) = project {
        if !is_project_id(id) {
            return Err(StoreError::InvalidProject(id.to_string()));
        }
    }
    Ok(dive_config::secrets::store_account(name, project))
}

#[derive(Clone)]
pub struct ChainedSource {
    sources: Vec<Arc<dyn SecretSource>>,
}

impl ChainedSource {
    pub fn new(sources: Vec<Arc<dyn SecretSource>>) -> Self {
        Self { sources }
    }
}

impl std::fmt::Debug for ChainedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainedSource")
            .field("sources", &self.sources.len())
            .finish()
    }
}

impl SecretSource for ChainedSource {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        self.resolve_in(name, None)
    }

    fn resolve_in(&self, name: &str, project: Option<&str>) -> Option<Resolved> {
        let mut empty = None;
        for source in &self.sources {
            match source.resolve_in(name, project) {
                Some(found) if !found.value.is_empty() => return Some(found),
                Some(found) => {
                    empty.get_or_insert(found);
                }
                None => {}
            }
        }
        empty
    }
}

pub fn system() -> ChainedSource {
    ChainedSource::new(vec![
        Arc::new(EnvSource),
        Arc::new(CredentialStore::system()),
    ])
}

pub fn in_project(project: Option<String>) -> InProject<ChainedSource> {
    InProject::new(system(), project)
}

pub fn for_config(paths: &[std::path::PathBuf]) -> InProject<ChainedSource> {
    in_project(dive_config::project_id(paths))
}

pub fn store_resolve(store: &dyn SecretStore, name: &str) -> Option<Resolved> {
    store_resolve_in(store, name, None)
}

pub fn store_resolve_in(
    store: &dyn SecretStore,
    name: &str,
    project: Option<&str>,
) -> Option<Resolved> {
    if !is_secret_name(name) {
        return None;
    }
    let found = |project| {
        store
            .get_in(name, project)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
    };
    project
        .and_then(|id| found(Some(id)))
        .or_else(|| found(None))
        .map(|value| Resolved {
            value,
            source: Origin::Store,
        })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NeededSecret {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub fn is_set(source: &dyn SecretSource, name: &str) -> bool {
    source
        .resolve(name)
        .is_some_and(|found| !found.value.is_empty())
}

pub fn env_references(text: &str) -> Vec<EnvReference<'_>> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            break;
        };
        let inner = &after[..end];
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        found.push(EnvReference { name, default });
        rest = &after[end + 1..];
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvReference<'a> {
    pub name: &'a str,
    pub default: Option<&'a str>,
}

#[cfg(test)]
mod tests;
