use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Env,
    Store,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Resolved {
    pub value: String,
    pub source: Origin,
}

impl fmt::Debug for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolved")
            .field("value", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

pub trait SecretSource: Send + Sync + fmt::Debug {
    fn resolve(&self, name: &str) -> Option<Resolved>;

    fn resolve_in(&self, name: &str, _project: Option<&str>) -> Option<Resolved> {
        self.resolve(name)
    }
}

impl<T: SecretSource + ?Sized> SecretSource for &T {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        (**self).resolve(name)
    }

    fn resolve_in(&self, name: &str, project: Option<&str>) -> Option<Resolved> {
        (**self).resolve_in(name, project)
    }
}

impl<T: SecretSource + ?Sized> SecretSource for Arc<T> {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        (**self).resolve(name)
    }

    fn resolve_in(&self, name: &str, project: Option<&str>) -> Option<Resolved> {
        (**self).resolve_in(name, project)
    }
}

impl<T: SecretSource + ?Sized> SecretSource for Box<T> {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        (**self).resolve(name)
    }

    fn resolve_in(&self, name: &str, project: Option<&str>) -> Option<Resolved> {
        (**self).resolve_in(name, project)
    }
}

#[derive(Debug, Clone)]
pub struct InProject<S> {
    inner: S,
    project: Option<String>,
}

impl<S: SecretSource> InProject<S> {
    pub fn new(inner: S, project: Option<String>) -> Self {
        Self { inner, project }
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }
}

impl<S: SecretSource> SecretSource for InProject<S> {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        self.inner.resolve_in(name, self.project.as_deref())
    }

    fn resolve_in(&self, name: &str, project: Option<&str>) -> Option<Resolved> {
        self.inner
            .resolve_in(name, project.or(self.project.as_deref()))
    }
}

pub fn is_project_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|ch| ch == '-' || ch.is_ascii_alphanumeric())
}

pub fn store_account(name: &str, project: Option<&str>) -> String {
    match project {
        Some(id) => format!("{name}@{id}"),
        None => name.to_string(),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSource;

impl SecretSource for EnvSource {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        if !is_secret_name(name) {
            return None;
        }
        std::env::var(name).ok().map(|value| Resolved {
            value,
            source: Origin::Env,
        })
    }
}

pub fn is_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_shell_variable_rules() {
        for good in ["A", "_A", "ANTHROPIC_API_KEY", "key2"] {
            assert!(is_secret_name(good), "{good}");
        }
        for bad in ["", "2KEY", "A-B", "A B", "FOO:-bar", "A.B"] {
            assert!(!is_secret_name(bad), "{bad}");
        }
    }

    #[test]
    fn project_ids_are_letters_digits_and_hyphens() {
        for good in ["6f1c2a0e-8d4b-4f5e-9a3c-2b7d1e0f4a59", "site", "a-1"] {
            assert!(is_project_id(good), "{good}");
        }
        for bad in ["", "a b", "a@b", "a/b", "a.b"] {
            assert!(!is_project_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_project_account_carries_the_project_id_after_an_at() {
        assert_eq!(store_account("SLACK_BOT_TOKEN", None), "SLACK_BOT_TOKEN");
        assert_eq!(
            store_account("SLACK_BOT_TOKEN", Some("p1")),
            "SLACK_BOT_TOKEN@p1"
        );
    }

    #[test]
    fn a_resolved_value_never_shows_in_debug_output() {
        let resolved = Resolved {
            value: "sk-very-secret".into(),
            source: Origin::Store,
        };
        let debug = format!("{resolved:?}");
        assert!(!debug.contains("sk-very-secret"));
        assert!(debug.contains("Store"));
    }
}
