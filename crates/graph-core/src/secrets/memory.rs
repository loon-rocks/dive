use super::{is_secret_name, Origin, Resolved, SecretSource, SecretStore, StoreError};
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
    fn check(&self, name: &str) -> Result<(), StoreError> {
        if !is_secret_name(name) {
            return Err(StoreError::InvalidName(name.to_string()));
        }
        self.status()
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

    fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        self.check(name)?;
        Ok(self.values.lock().unwrap().get(name).cloned())
    }

    fn set(&self, name: &str, value: &str) -> Result<(), StoreError> {
        self.check(name)?;
        self.values
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<bool, StoreError> {
        self.check(name)?;
        Ok(self.values.lock().unwrap().remove(name).is_some())
    }
}

impl SecretSource for MemorySecrets {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        self.get(name).ok().flatten().map(|value| Resolved {
            value,
            source: self.source,
        })
    }
}
