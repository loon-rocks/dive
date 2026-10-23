use super::{store_account, store_resolve_in, Resolved, SecretSource, SecretStore, StoreError};

pub const SERVICE: &str = "dive";
pub const LEGACY_SERVICE: &str = "graph";
pub const STORE_ENV: &str = "DIVE_SECRET_STORE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialStore {
    enabled: bool,
}

impl CredentialStore {
    pub fn system() -> Self {
        Self {
            enabled: !matches!(dive_config::legacy::var(STORE_ENV).as_deref(), Some("none")),
        }
    }

    pub fn disabled() -> Self {
        Self { enabled: false }
    }

    fn entry(&self, name: &str, project: Option<&str>) -> Result<keyring::Entry, StoreError> {
        self.entry_in(SERVICE, name, project)
    }

    fn entry_in(
        &self,
        service: &str,
        name: &str,
        project: Option<&str>,
    ) -> Result<keyring::Entry, StoreError> {
        let account = store_account(name, project)?;
        self.status()?;
        keyring::Entry::new(service, &account).map_err(describe)
    }

    fn password(entry: keyring::Entry) -> Result<Option<String>, StoreError> {
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(describe(error)),
        }
    }

    fn delete(entry: keyring::Entry) -> Result<bool, StoreError> {
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(describe(error)),
        }
    }

    fn accounts_in(service: &str) -> Result<Vec<String>, StoreError> {
        let spec = std::collections::HashMap::from([("service", service)]);
        let entries = match keyring_core::Entry::search(&spec) {
            Ok(entries) => entries,
            Err(keyring::Error::NoEntry) => return Ok(Vec::new()),
            Err(error) => return Err(describe(error)),
        };
        Ok(entries
            .iter()
            .filter_map(|entry| entry.get_specifiers())
            .filter(|(found, _)| found == service)
            .map(|(_, account)| account)
            .collect())
    }
}

fn describe(error: keyring::Error) -> StoreError {
    match error {
        keyring::Error::NoDefaultStore => {
            StoreError::Unavailable("no OS credential store could be opened".to_string())
        }
        keyring::Error::NoStorageAccess(cause) => StoreError::Unavailable(cause.to_string()),
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => {
            StoreError::Failed("the stored value is not valid UTF-8 text".to_string())
        }
        keyring::Error::Ambiguous(found) => StoreError::Failed(format!(
            "{} stored credentials match this name",
            found.len()
        )),
        other => StoreError::Failed(other.to_string()),
    }
}

impl SecretStore for CredentialStore {
    fn status(&self) -> Result<(), StoreError> {
        if !self.enabled {
            return Err(StoreError::Unavailable(format!("{STORE_ENV} is none")));
        }
        keyring::Entry::store_status()
            .as_ref()
            .map_err(|_| StoreError::Unavailable("no OS credential store could be opened".into()))
            .map(|_| ())
    }

    fn get_in(&self, name: &str, project: Option<&str>) -> Result<Option<String>, StoreError> {
        if let Some(value) = Self::password(self.entry(name, project)?)? {
            return Ok(Some(value));
        }
        let legacy = Self::password(self.entry_in(LEGACY_SERVICE, name, project)?)?;
        if legacy.is_some() {
            dive_config::legacy::note(format!(
                "{name} is stored under the credential store service `{LEGACY_SERVICE}`, which is deprecated; run `dive secret set {name}` to store it under `{SERVICE}`"
            ));
        }
        Ok(legacy)
    }

    fn set_in(&self, name: &str, project: Option<&str>, value: &str) -> Result<(), StoreError> {
        self.entry(name, project)?
            .set_password(value)
            .map_err(describe)
    }

    fn remove_in(&self, name: &str, project: Option<&str>) -> Result<bool, StoreError> {
        let current = Self::delete(self.entry(name, project)?)?;
        let legacy = Self::delete(self.entry_in(LEGACY_SERVICE, name, project)?)?;
        Ok(current || legacy)
    }

    fn accounts(&self) -> Result<Vec<String>, StoreError> {
        self.status()?;
        let mut accounts = Self::accounts_in(SERVICE)?;
        accounts.extend(Self::accounts_in(LEGACY_SERVICE)?);
        accounts.sort();
        accounts.dedup();
        Ok(accounts)
    }
}

impl SecretSource for CredentialStore {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        store_resolve_in(self, name, None)
    }

    fn resolve_in(&self, name: &str, project: Option<&str>) -> Option<Resolved> {
        store_resolve_in(self, name, project)
    }
}
