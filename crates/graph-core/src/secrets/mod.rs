mod memory;
mod store;

pub use graph_config::secrets::{is_secret_name, EnvSource, Origin, Resolved, SecretSource};
pub use memory::MemorySecrets;
pub use store::{CredentialStore, SERVICE, STORE_ENV};

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("the OS credential store is unavailable: {0}")]
    Unavailable(String),
    #[error("{0} is not a variable name: use letters, digits and underscores, not starting with a digit")]
    InvalidName(String),
    #[error("the OS credential store failed: {0}")]
    Failed(String),
}

pub trait SecretStore: SecretSource {
    fn status(&self) -> Result<(), StoreError>;
    fn get(&self, name: &str) -> Result<Option<String>, StoreError>;
    fn set(&self, name: &str, value: &str) -> Result<(), StoreError>;
    fn remove(&self, name: &str) -> Result<bool, StoreError>;
}

impl<T: SecretStore + ?Sized> SecretStore for Arc<T> {
    fn status(&self) -> Result<(), StoreError> {
        (**self).status()
    }

    fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        (**self).get(name)
    }

    fn set(&self, name: &str, value: &str) -> Result<(), StoreError> {
        (**self).set(name, value)
    }

    fn remove(&self, name: &str) -> Result<bool, StoreError> {
        (**self).remove(name)
    }
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
        let mut empty = None;
        for source in &self.sources {
            match source.resolve(name) {
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

pub fn store_resolve(store: &dyn SecretStore, name: &str) -> Option<Resolved> {
    store.get(name).ok().flatten().map(|value| Resolved {
        value,
        source: Origin::Store,
    })
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
