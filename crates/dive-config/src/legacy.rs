use crate::load::expand_tilde;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

pub const PROJECT_DIR: &str = ".dive";
pub const LEGACY_PROJECT_DIR: &str = ".graph";
pub const LEGACY_NAME: &str = "graph";
const NAME: &str = "dive";
const ENV_PREFIX: &str = "DIVE_";
const LEGACY_ENV_PREFIX: &str = "GRAPH_";

static USED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

pub fn note(notice: String) {
    if let Ok(mut used) = USED.lock() {
        used.insert(notice);
    }
}

pub fn notices() -> Vec<String> {
    USED.lock()
        .map(|used| used.iter().cloned().collect())
        .unwrap_or_default()
}

pub fn legacy_path(path: &Path) -> Option<PathBuf> {
    let mut changed = false;
    let mut previous: Option<OsString> = None;
    let mut mapped = PathBuf::new();
    for component in path.components() {
        let name = component.as_os_str();
        let renamed = match component {
            Component::Normal(_) if name == PROJECT_DIR => Some(LEGACY_PROJECT_DIR),
            Component::Normal(_)
                if name == NAME
                    && previous
                        .as_deref()
                        .is_some_and(|parent| parent == ".config" || parent == "share") =>
            {
                Some(LEGACY_NAME)
            }
            _ => None,
        };
        match renamed {
            Some(legacy) => {
                changed = true;
                mapped.push(legacy);
            }
            None => mapped.push(name),
        }
        previous = Some(name.to_os_string());
    }
    changed.then_some(mapped)
}

pub fn resolve(path: &Path) -> PathBuf {
    let current = expand_tilde(path);
    if current.exists() {
        return path.to_path_buf();
    }
    let Some(legacy) = legacy_path(path) else {
        return path.to_path_buf();
    };
    let expanded = expand_tilde(&legacy);
    if !expanded.exists() {
        return path.to_path_buf();
    }
    note(format!(
        "{} is deprecated; rename it to {}",
        expanded.display(),
        current.display()
    ));
    legacy
}

pub fn project_dir(root: &Path) -> PathBuf {
    resolve(&root.join(PROJECT_DIR))
}

pub fn var(name: &str) -> Option<String> {
    var_os(name).and_then(|value| value.into_string().ok())
}

pub fn var_os(name: &str) -> Option<OsString> {
    if let Some(value) = std::env::var_os(name) {
        return Some(value);
    }
    let legacy = legacy_var_name(name)?;
    let value = std::env::var_os(&legacy)?;
    note(format!("{legacy} is deprecated; set {name} instead"));
    Some(value)
}

fn legacy_var_name(name: &str) -> Option<String> {
    name.strip_prefix(ENV_PREFIX)
        .map(|rest| format!("{LEGACY_ENV_PREFIX}{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_path_maps_project_config_and_data_dirs() {
        assert_eq!(
            legacy_path(Path::new("./.dive/plans")),
            Some(PathBuf::from("./.graph/plans"))
        );
        assert_eq!(
            legacy_path(Path::new("~/.config/dive/tools")),
            Some(PathBuf::from("~/.config/graph/tools"))
        );
        assert_eq!(
            legacy_path(Path::new("~/.local/share/dive")),
            Some(PathBuf::from("~/.local/share/graph"))
        );
        assert_eq!(legacy_path(Path::new("/srv/dive/plans")), None);
        assert_eq!(legacy_path(Path::new("plans")), None);
    }

    #[test]
    fn resolve_prefers_the_new_dir_and_falls_back_to_an_existing_legacy_one() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(".dive").join("plans");
        let legacy = dir.path().join(".graph").join("plans");

        assert_eq!(resolve(&current), current);

        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(resolve(&current), legacy);
        assert!(notices()
            .iter()
            .any(|notice| notice.contains(".graph") && notice.contains("rename it to")));

        std::fs::create_dir_all(&current).unwrap();
        assert_eq!(resolve(&current), current);
    }

    #[test]
    fn var_reads_the_legacy_name_when_the_new_one_is_unset() {
        std::env::set_var("GRAPH_LEGACY_TEST_ONLY_OLD", "old");
        assert_eq!(var("DIVE_LEGACY_TEST_ONLY_OLD").as_deref(), Some("old"));
        assert!(notices()
            .iter()
            .any(|notice| notice.contains("GRAPH_LEGACY_TEST_ONLY_OLD is deprecated")));

        std::env::set_var("GRAPH_LEGACY_TEST_BOTH", "old");
        std::env::set_var("DIVE_LEGACY_TEST_BOTH", "new");
        assert_eq!(var("DIVE_LEGACY_TEST_BOTH").as_deref(), Some("new"));

        assert_eq!(var("DIVE_LEGACY_TEST_NEVER_SET"), None);
    }
}
