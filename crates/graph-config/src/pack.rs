use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PACK_FORMAT: u32 = 1;
pub const PACK_FORMAT_OLDEST: u32 = 1;
pub const LOCK_FORMAT: u32 = 1;
pub const LOCK_FORMAT_OLDEST: u32 = 1;

pub const MANIFEST_FILE: &str = "pack.toml";
pub const LOCK_FILE: &str = "packs.lock";
pub const PACKS_DIR: &str = "packs";

pub const MAX_PACK_NAME: usize = 20;
pub const MAX_CATALOG_NAME: usize = 64;
pub const RESERVED_NAMES: &[&str] = &["user", "plan", "agent", "builtin", "workbench", "pack"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackEntry {
    pub git: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Global,
    Project,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PackLayer {
    pub kind: LayerKind,
    pub dir: PathBuf,
    pub packs: BTreeMap<String, PackEntry>,
}

impl PackLayer {
    pub fn packs_dir(&self) -> PathBuf {
        self.dir.join(PACKS_DIR)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(LOCK_FILE)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackManifest {
    pub version: u32,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default)]
    pub requires: PackRequires,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackRequires {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binaries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpRequirement>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpRequirement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackLock {
    pub version: u32,
    #[serde(default)]
    pub packs: BTreeMap<String, LockEntry>,
}

impl Default for PackLock {
    fn default() -> Self {
        PackLock {
            version: LOCK_FORMAT,
            packs: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockEntry {
    pub git: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub sha: String,
    pub hash: String,
}

pub fn parse_manifest(text: &str) -> Result<PackManifest, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

pub fn load_manifest(path: &Path) -> Result<PackManifest, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_manifest(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse_lock(text: &str) -> Result<PackLock, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

pub fn load_lock(path: &Path) -> Result<PackLock, String> {
    if !path.exists() {
        return Ok(PackLock::default());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_lock(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn check_pack_name(name: &str) -> Result<(), String> {
    let well_formed = name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !well_formed {
        return Err(format!(
            "pack name '{name}' must start with a lowercase letter and use only lowercase letters, digits and underscores"
        ));
    }
    if name.contains("__") {
        return Err(format!("pack name '{name}' must not contain '__'"));
    }
    if name.len() > MAX_PACK_NAME {
        return Err(format!(
            "pack name '{name}' is {} characters; the limit is {MAX_PACK_NAME}",
            name.len()
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(format!("pack name '{name}' is reserved"));
    }
    Ok(())
}
