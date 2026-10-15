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
}

impl<T: SecretSource + ?Sized> SecretSource for &T {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        (**self).resolve(name)
    }
}

impl<T: SecretSource + ?Sized> SecretSource for Arc<T> {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        (**self).resolve(name)
    }
}

impl<T: SecretSource + ?Sized> SecretSource for Box<T> {
    fn resolve(&self, name: &str) -> Option<Resolved> {
        (**self).resolve(name)
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
