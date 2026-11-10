use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dive_core::requirements::{Item, Missing, Requirements};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Project,
    Global,
    Pack,
    Builtin,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
            Scope::Pack => "pack",
            Scope::Builtin => "built-in",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListContext {
    pub requirements: Requirements,
    pub root: Option<PathBuf>,
    pub global_dir: PathBuf,
    pub servers: BTreeMap<String, Scope>,
    pub user_tools: BTreeMap<String, PathBuf>,
}

impl ListContext {
    pub fn scope_of_path(&self, path: &Path) -> Scope {
        of_path(path, self.root.as_deref(), &self.global_dir)
    }

    pub fn missing(&self, item: &Item) -> Vec<Missing> {
        self.requirements.missing(item).to_vec()
    }
}

pub fn of_path(path: &Path, root: Option<&Path>, global_dir: &Path) -> Scope {
    let Some(root) = root else {
        return Scope::Global;
    };
    let absolute =
        std::path::absolute(dive_config::expand_tilde(path)).unwrap_or_else(|_| path.to_path_buf());
    if absolute.starts_with(global_dir) {
        return Scope::Global;
    }
    if path.is_relative() || absolute.starts_with(root) {
        return Scope::Project;
    }
    Scope::Global
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn global() -> PathBuf {
        PathBuf::from("/home/me/.config/dive")
    }

    fn root() -> PathBuf {
        PathBuf::from("/work/site")
    }

    #[test]
    fn a_file_under_the_global_config_dir_is_global() {
        let path = Path::new("/home/me/.config/dive/plans/read_sources.yaml");
        assert_eq!(of_path(path, Some(&root()), &global()), Scope::Global);
    }

    #[test]
    fn a_file_under_the_workspace_root_is_project() {
        let path = Path::new("/work/site/.dive/plans/deploy_check.yaml");
        assert_eq!(of_path(path, Some(&root()), &global()), Scope::Project);
    }

    #[test]
    fn a_relative_file_is_project() {
        let path = Path::new("./.dive/plans/deploy_check.yaml");
        assert_eq!(of_path(path, Some(&root()), &global()), Scope::Project);
    }

    #[test]
    fn a_shared_directory_outside_the_workspace_is_global() {
        let path = Path::new("/srv/plans/shared.yaml");
        assert_eq!(of_path(path, Some(&root()), &global()), Scope::Global);
    }

    #[test]
    fn every_file_is_global_in_the_global_workspace() {
        let path = Path::new("./.dive/plans/deploy_check.yaml");
        assert_eq!(of_path(path, None, &global()), Scope::Global);
    }
}
