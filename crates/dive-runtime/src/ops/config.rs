use crate::packs::{InstalledPacks, PackCheck};
use crate::runtime::Workspace;
use anyhow::{bail, Context, Result};
use dive_config::{LoadedConfig, Migrated, CONFIG_FORMAT, CONFIG_FORMAT_OLDEST, FORMAT_KEY};
use serde::Serialize;
use std::path::PathBuf;

const STARTER_CONFIG: &str = include_str!("starter_config.toml");

pub fn starter_config() -> String {
    format!("{FORMAT_KEY} = {CONFIG_FORMAT}\n\n{STARTER_CONFIG}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayerVerdict {
    pub path: String,
    pub version: u32,
    pub declared: Option<u32>,
    pub current: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigShow {
    pub version: u32,
    pub config: dive_config::Config,
    pub sources: Vec<String>,
    pub layers: Vec<LayerVerdict>,
    #[serde(skip)]
    pub toml: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigFile {
    pub path: String,
    pub exists: bool,
    pub version: Option<u32>,
}

impl ConfigFile {
    pub fn marker(&self) -> String {
        match (self.version, self.exists) {
            (Some(version), _) => format!("exists\tversion {version}"),
            (None, true) => "exists".to_string(),
            (None, false) => "missing".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigPaths {
    pub files: Vec<ConfigFile>,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum FileCheck {
    Missing {
        path: String,
        exists: bool,
    },
    Readable {
        path: String,
        exists: bool,
        version: u32,
        declared: Option<u32>,
        readable: bool,
        current: bool,
    },
    Unreadable {
        path: String,
        exists: bool,
        error: String,
    },
}

impl FileCheck {
    pub fn path(&self) -> &str {
        match self {
            FileCheck::Missing { path, .. }
            | FileCheck::Readable { path, .. }
            | FileCheck::Unreadable { path, .. } => path,
        }
    }

    pub fn line(&self) -> String {
        match self {
            FileCheck::Missing { path, .. } => format!("{path}\tmissing\n"),
            FileCheck::Unreadable { path, .. } => format!("{path}\tunreadable\n"),
            FileCheck::Readable {
                path,
                version,
                declared,
                readable,
                ..
            } => {
                let verdict = if !readable {
                    if *version > CONFIG_FORMAT {
                        "too new"
                    } else {
                        "too old"
                    }
                } else if *version < CONFIG_FORMAT {
                    "migrate"
                } else if declared.is_none() {
                    "current (unstamped)"
                } else {
                    "current"
                };
                format!("{path}\tversion {version}\t{verdict}\n")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigCheck {
    pub ok: bool,
    pub version: u32,
    pub files: Vec<FileCheck>,
    pub problems: Vec<String>,
    pub packs: Vec<PackCheck>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    pub packs_text: String,
}

impl ConfigCheck {
    pub fn text(&self) -> String {
        let mut text: String = self.files.iter().map(FileCheck::line).collect();
        text.push_str(&self.packs_text);
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigInit {
    pub ok: bool,
    pub saved_to: String,
}

pub fn candidates(workspace: &Workspace) -> Vec<PathBuf> {
    workspace
        .config_paths()
        .iter()
        .map(|path| dive_config::expand_tilde(path))
        .collect()
}

fn target(workspace: &Workspace, global: bool) -> PathBuf {
    let paths = candidates(workspace);
    if global {
        paths[0].clone()
    } else {
        paths[1].clone()
    }
}

pub fn show(loaded: &LoadedConfig) -> Result<ConfigShow> {
    Ok(ConfigShow {
        version: CONFIG_FORMAT,
        config: loaded.config.clone(),
        sources: loaded
            .sources
            .iter()
            .map(|source| source.display().to_string())
            .collect(),
        layers: loaded
            .layers
            .iter()
            .map(|layer| LayerVerdict {
                path: layer.path.display().to_string(),
                version: layer.version,
                declared: layer.declared,
                current: layer.version == CONFIG_FORMAT,
                notes: layer.notes.clone(),
            })
            .collect(),
        toml: format!(
            "{FORMAT_KEY} = {CONFIG_FORMAT}\n\n{}",
            toml::to_string_pretty(&loaded.config)?
        ),
    })
}

pub fn path(workspace: &Workspace) -> ConfigPaths {
    let files: Vec<ConfigFile> = candidates(workspace)
        .into_iter()
        .map(|candidate| {
            let exists = candidate.exists();
            ConfigFile {
                path: candidate.display().to_string(),
                exists,
                version: exists
                    .then(|| dive_config::inspect(&candidate).ok())
                    .flatten()
                    .map(|info| info.version),
            }
        })
        .collect();
    ConfigPaths {
        count: files.len(),
        files,
    }
}

pub async fn check(workspace: &Workspace) -> ConfigCheck {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    for candidate in candidates(workspace) {
        let path = candidate.display().to_string();
        if !candidate.exists() {
            files.push(FileCheck::Missing {
                path,
                exists: false,
            });
            continue;
        }
        match dive_config::inspect(&candidate) {
            Ok(info) => {
                if let Some(problem) = dive_config::window_problem(
                    "config",
                    &candidate.display(),
                    info.version,
                    CONFIG_FORMAT_OLDEST,
                    CONFIG_FORMAT,
                ) {
                    problems.push(problem);
                }
                files.push(FileCheck::Readable {
                    path,
                    exists: true,
                    version: info.version,
                    declared: info.declared,
                    readable: (CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT).contains(&info.version),
                    current: info.version == CONFIG_FORMAT,
                });
            }
            Err(error) => {
                problems.push(error.clone());
                files.push(FileCheck::Unreadable {
                    path,
                    exists: true,
                    error,
                });
            }
        }
    }
    let mut packs = Vec::new();
    let mut packs_text = String::new();
    let mut warnings = Vec::new();
    if problems.is_empty() {
        match dive_config::load_from(&workspace.config_paths(), &dive_core::secrets::system()) {
            Err(error) => problems.push(format!("{error:#}")),
            Ok(loaded) => {
                warnings.extend(loaded.layers.iter().flat_map(|layer| {
                    layer
                        .notes
                        .iter()
                        .map(|note| format!("{}: {note}", layer.path.display()))
                }));
                match InstalledPacks::for_config(&loaded) {
                    Ok(installed) => {
                        let manager = dive_mcp::McpManager::new(loaded.config.mcp.clone());
                        let unmet = installed.unmet(&manager).await;
                        manager.shutdown().await;
                        packs_text = installed.check_text(&unmet);
                        packs = installed.check_entries(&unmet);
                        warnings.extend(installed.warnings);
                        warnings.extend(dive_core::packs::requirements::describe_unmet(&unmet));
                    }
                    Err(error) => problems.push(format!("{error:#}")),
                }
            }
        }
    }
    let ok = problems.is_empty();
    ConfigCheck {
        ok,
        version: CONFIG_FORMAT,
        files,
        problems,
        packs,
        warnings,
        error: (!ok).then(|| "config does not load".to_string()),
        packs_text,
    }
}

pub fn migrate(workspace: &Workspace, global: bool) -> Result<Migrated> {
    let target = target(workspace, global);
    if !target.exists() {
        bail!(
            "{} does not exist (run `dive config init{}` to create it)",
            target.display(),
            if global { " --global" } else { "" }
        );
    }
    dive_config::migrate_file(&target).map_err(anyhow::Error::msg)
}

pub fn init(workspace: &Workspace, global: bool, force: bool) -> Result<ConfigInit> {
    let target = target(workspace, global);
    if target.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&target, starter_config())
        .with_context(|| format!("writing {}", target.display()))?;
    Ok(ConfigInit {
        ok: true,
        saved_to: target.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(".dive").join("config.toml")
    }

    #[test]
    fn starter_parses_and_leaves_prompts_to_the_agent_files() {
        let starter = starter_config();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, &starter).unwrap();
        let loaded = dive_config::load_from(&[path], &dive_core::secrets::EnvSource).unwrap();
        let config = loaded.config;
        assert_eq!(config.prompts.chat, None);
        assert_eq!(config.prompts.workbench, None);
        assert_eq!(loaded.layers[0].declared, Some(CONFIG_FORMAT));
        assert!(starter.starts_with(&format!("{FORMAT_KEY} = {CONFIG_FORMAT}\n")));
    }

    #[test]
    fn init_writes_the_project_layer_and_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        let created = init(&workspace, false, false).unwrap();
        assert_eq!(created.saved_to, project(dir.path()).display().to_string());
        assert_eq!(
            serde_json::to_value(&created).unwrap(),
            json!({"ok": true, "savedTo": created.saved_to})
        );
        let error = init(&workspace, false, false).unwrap_err().to_string();
        assert!(error.ends_with("already exists (use --force to overwrite)"));
        assert!(init(&workspace, false, true).is_ok());
    }

    #[test]
    fn path_and_check_report_the_project_layer() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        let listed = path(&workspace);
        assert_eq!(listed.count, 2);
        assert_eq!(listed.files[1].marker(), "missing");

        std::fs::create_dir_all(dir.path().join(".dive")).unwrap();
        std::fs::write(project(dir.path()), "[settings]\n").unwrap();
        let listed = path(&workspace);
        assert_eq!(listed.files[1].version, Some(1));
        assert_eq!(listed.files[1].marker(), "exists\tversion 1");
        assert_eq!(
            serde_json::to_value(&listed.files[1]).unwrap(),
            json!({"path": listed.files[1].path, "exists": true, "version": 1})
        );
    }

    #[tokio::test]
    async fn check_rejects_a_layer_from_a_newer_binary() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        std::fs::create_dir_all(dir.path().join(".dive")).unwrap();
        std::fs::write(
            project(dir.path()),
            format!("version = {}\n", CONFIG_FORMAT + 1),
        )
        .unwrap();
        let verdict = check(&workspace).await;
        assert!(!verdict.ok);
        assert_eq!(verdict.error.as_deref(), Some("config does not load"));
        let readable = verdict
            .files
            .iter()
            .find(|file| file.path().ends_with("config.toml") && file.path().contains(".dive"))
            .unwrap();
        assert!(readable.line().ends_with("\ttoo new\n"));
        let value = serde_json::to_value(&verdict).unwrap();
        assert_eq!(value["packs"], json!([]));
        assert_eq!(value["error"], json!("config does not load"));
    }

    #[tokio::test]
    async fn check_passes_a_current_project_layer_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        std::fs::create_dir_all(dir.path().join(".dive")).unwrap();
        std::fs::write(
            project(dir.path()),
            format!(
                "version = {CONFIG_FORMAT}\n[settings]\ndata_dir = \"{}\"\n",
                dir.path().join("data").display()
            ),
        )
        .unwrap();
        let verdict = check(&workspace).await;
        let project_line = verdict
            .files
            .iter()
            .map(FileCheck::line)
            .find(|line| line.contains(".dive"))
            .unwrap();
        assert!(project_line.ends_with("\tcurrent\n"), "{project_line}");
        if verdict.ok {
            assert!(serde_json::to_value(&verdict)
                .unwrap()
                .get("error")
                .is_none());
        }
    }

    #[test]
    fn migrate_names_the_init_command_when_the_layer_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        let error = migrate(&workspace, false).unwrap_err().to_string();
        assert!(
            error.ends_with("does not exist (run `dive config init` to create it)"),
            "{error}"
        );
        std::fs::create_dir_all(dir.path().join(".dive")).unwrap();
        std::fs::write(project(dir.path()), "[settings]\n").unwrap();
        let migrated = migrate(&workspace, false).unwrap();
        assert_eq!((migrated.from, migrated.to), (1, CONFIG_FORMAT));
        assert!(migrated.changed);
    }

    #[test]
    fn show_renders_toml_with_the_version_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let loaded =
            dive_config::load_from(&[project(dir.path())], &dive_core::secrets::EnvSource).unwrap();
        let shown = show(&loaded).unwrap();
        assert!(shown
            .toml
            .starts_with(&format!("{FORMAT_KEY} = {CONFIG_FORMAT}\n\n")));
        let value = serde_json::to_value(&shown).unwrap();
        assert_eq!(
            value.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["config", "layers", "sources", "version"]
        );
    }
}
