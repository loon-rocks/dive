use super::{is_project_id, StoreError};
use std::collections::BTreeMap;
use std::sync::Mutex;

pub const SIGN_IN_SERVICE: &str = "dive-oauth";

pub trait SignInStore: Send + Sync {
    fn available(&self) -> Result<(), StoreError>;
    fn load(&self, url: &str, project: Option<&str>) -> Result<Option<String>, StoreError>;
    fn save(&self, url: &str, project: Option<&str>, value: &str) -> Result<(), StoreError>;
    fn remove(&self, url: &str, project: Option<&str>) -> Result<bool, StoreError>;
}

pub fn sign_in_account(url: &str, project: Option<&str>) -> Result<String, StoreError> {
    match project {
        Some(id) if !is_project_id(id) => Err(StoreError::InvalidProject(id.to_string())),
        Some(id) => Ok(format!("{id} {url}")),
        None => Ok(url.to_string()),
    }
}

pub struct MemorySignIns {
    values: Mutex<BTreeMap<String, String>>,
    available: bool,
}

impl MemorySignIns {
    pub fn new() -> Self {
        Self {
            values: Mutex::new(BTreeMap::new()),
            available: true,
        }
    }

    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::new()
        }
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.available {
            Ok(())
        } else {
            Err(StoreError::Unavailable("memory store disabled".to_string()))
        }
    }
}

impl Default for MemorySignIns {
    fn default() -> Self {
        Self::new()
    }
}

impl SignInStore for MemorySignIns {
    fn available(&self) -> Result<(), StoreError> {
        self.check()
    }

    fn load(&self, url: &str, project: Option<&str>) -> Result<Option<String>, StoreError> {
        self.check()?;
        let account = sign_in_account(url, project)?;
        Ok(self.values.lock().unwrap().get(&account).cloned())
    }

    fn save(&self, url: &str, project: Option<&str>, value: &str) -> Result<(), StoreError> {
        self.check()?;
        let account = sign_in_account(url, project)?;
        self.values
            .lock()
            .unwrap()
            .insert(account, value.to_string());
        Ok(())
    }

    fn remove(&self, url: &str, project: Option<&str>) -> Result<bool, StoreError> {
        self.check()?;
        let account = sign_in_account(url, project)?;
        Ok(self.values.lock().unwrap().remove(&account).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_and_global_sign_ins_are_separate_accounts() {
        let store = MemorySignIns::new();
        store
            .save("https://mcp.example/mcp", None, "global")
            .unwrap();
        store
            .save("https://mcp.example/mcp", Some("acme"), "project")
            .unwrap();
        assert_eq!(
            store
                .load("https://mcp.example/mcp", None)
                .unwrap()
                .as_deref(),
            Some("global")
        );
        assert_eq!(
            store
                .load("https://mcp.example/mcp", Some("acme"))
                .unwrap()
                .as_deref(),
            Some("project")
        );
        assert_eq!(
            store
                .load("https://mcp.example/mcp", Some("other"))
                .unwrap(),
            None
        );
        assert!(store
            .remove("https://mcp.example/mcp", Some("acme"))
            .unwrap());
        assert!(!store
            .remove("https://mcp.example/mcp", Some("acme"))
            .unwrap());
    }

    #[test]
    fn a_bad_project_id_is_refused() {
        assert!(matches!(
            sign_in_account("https://x", Some("not a project")),
            Err(StoreError::InvalidProject(_))
        ));
    }
}
