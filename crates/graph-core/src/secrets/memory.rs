use super::{
    store_account, store_resolve_in, Origin, Resolved, SecretSource, SecretStore, StoreError,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

pub struct MemorySecrets {
    values: Mutex<BTreeMap<String, String>>,
    source: Origin,
    available: bool,
}

impl MemorySecrets {
    pub fn store() -> Self {
        Self {
            values: Mutex::new(BTreeMap::new()),
            source: Origin::Store,
            available: true,
        }
    }

    pub fn env() -> Self {
        Self {
            source: Origin::Env,
            ..Self::store()
        }
    }

    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::store()
        }
    }

    pub fn with_project(self, name: &str, project: &str, value: &str) -> Self {
        self.with(&format!("{name}@{project}"), value)
    }

    pub fn with(self, name: &str, value: &str) -> Self {
        self.values
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        self
    }

    pub fn names(&self) -> Vec<String> {
        self.values.lock().unwrap().keys().cloned().collect()
    }
}

impl Default for MemorySecrets {
    fn default() -> Self {
        Self::store()
    }
}

impl std::fmt::Debug for MemorySecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySecrets")
            .field("names", &self.names())
            .field("source", &self.source)
            .field("available", &self.available)
            .finish()
    }
}

impl MemorySecrets {
    fn account(&self, name: &str, project: Option<&str>) -> Result<String, StoreError> {
        let account = store_account(name, project)?;
        self.status()?;
        Ok(account)
    }
}

impl SecretStore for MemorySecrets {
    fn status(&self) -> Result<(), StoreError> {
        if self.available {
            Ok(())
        } else {
            Err(StoreError::Unavailable(
                "the in-memory store is switched off".into(),
            ))
        }
    }

    fn get_in(&self, name: &str, project: Option<&str>) -> Result<Option<String>, StoreError> {
        let account = self.account(name, project)?;
        Ok(self.values.lock().unwrap().get(&account).cloned())
    }

    fn set_in(&self, name: &str, project: Option<&str>, value: &str) -> Result<(), StoreError> {
        let account = self.account(name, project)?;
        self.values
            .lock()
            .unwrap()
            .insert(account, value.to_string());
        Ok(())
    }

    fn remove_in(&self, name: &str, project: Option<&str>) -> Result<bool, StoreError> {
        let account = self.account(name, project)?;
        Ok(self.values.lock().unwrap().remove(&account).is_some())
    }
}

impl SecretSource for MemorySecrets {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        self.resolve_in(name, None)
    }

    fn resolve_in(&self, name: &str, project: Option<&str>) -> Option<Resolved> {
        match self.source {
            Origin::Store => store_resolve_in(self, name, project),
            Origin::Env => self.get(name).ok().flatten().map(|value| Resolved {
                value,
                source: Origin::Env,
            }),
        }
    }
}
