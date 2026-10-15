use super::{store_account, store_resolve_in, Resolved, SecretSource, SecretStore, StoreError};

pub const SERVICE: &str = "graph";
pub const STORE_ENV: &str = "GRAPH_SECRET_STORE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialStore {
    enabled: bool,
}

impl CredentialStore {
    pub fn system() -> Self {
        Self {
            enabled: !matches!(std::env::var(STORE_ENV).as_deref(), Ok("none")),
        }
    }

    pub fn disabled() -> Self {
        Self { enabled: false }
    }

    fn entry(&self, name: &str, project: Option<&str>) -> Result<keyring::Entry, StoreError> {
        let account = store_account(name, project)?;
        self.status()?;
        keyring::Entry::new(SERVICE, &account).map_err(describe)
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
        match self.entry(name, project)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(describe(error)),
        }
    }

    fn set_in(&self, name: &str, project: Option<&str>, value: &str) -> Result<(), StoreError> {
        self.entry(name, project)?
            .set_password(value)
            .map_err(describe)
    }

    fn remove_in(&self, name: &str, project: Option<&str>) -> Result<bool, StoreError> {
        match self.entry(name, project)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(describe(error)),
        }
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
