use crate::format::{declared_version, window_problem, Migration, FORMAT_KEY};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
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

pub const PACK_KIND: &str = "pack";
pub const LOCK_KIND: &str = "lock";
pub const SUPPORTED_OS: &[&str] = &["macos", "linux"];
pub const HASH_PREFIX: &str = "sha256:";

const PACK_MIGRATIONS: &[Migration] = &[];
const LOCK_MIGRATIONS: &[Migration] = &[];

pub fn pack_migration_count() -> usize {
    PACK_MIGRATIONS.len()
}

pub fn lock_migration_count() -> usize {
    LOCK_MIGRATIONS.len()
}

struct Contract {
    kind: &'static str,
    oldest: u32,
    current: u32,
    migrations: &'static [Migration],
}

const MANIFEST_CONTRACT: Contract = Contract {
    kind: PACK_KIND,
    oldest: PACK_FORMAT_OLDEST,
    current: PACK_FORMAT,
    migrations: PACK_MIGRATIONS,
};

const LOCK_CONTRACT: Contract = Contract {
    kind: LOCK_KIND,
    oldest: LOCK_FORMAT_OLDEST,
    current: LOCK_FORMAT,
    migrations: LOCK_MIGRATIONS,
};

fn read_versioned<T: serde::de::DeserializeOwned>(
    text: &str,
    what: &dyn fmt::Display,
    contract: &Contract,
) -> Result<T, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("{what}: {e}"))?;
    let declared = declared_version(&doc)
        .map_err(|e| format!("{what}: {e}"))?
        .ok_or_else(|| {
            format!(
                "{what}: {FORMAT_KEY} is required; this graph writes {} version {}",
                contract.kind, contract.current
            )
        })?;
    if let Some(problem) = window_problem(
        contract.kind,
        what,
        declared,
        contract.oldest,
        contract.current,
    ) {
        return Err(problem);
    }
    for migration in &contract.migrations[(declared - contract.oldest) as usize..] {
        migration(&mut doc).map_err(|e| format!("{what}: {e}"))?;
    }
    doc[FORMAT_KEY] = toml_edit::value(i64::from(contract.current));
    toml::from_str(&doc.to_string()).map_err(|e| format!("{what}: {e}"))
}

fn semver_problem(field: &str, requirement: &str) -> Option<String> {
    VersionReq::parse(requirement)
        .err()
        .map(|e| format!("{field}: '{requirement}' is not a semver requirement ({e})"))
}

impl PackManifest {
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(e) = check_pack_name(&self.name) {
            problems.push(format!("name: {e}"));
        }
        if self.description.trim().is_empty() {
            problems.push("description: must not be empty".to_string());
        }
        if let Some(graph) = &self.requires.graph {
            problems.extend(semver_problem("requires.graph", graph));
        }
        for os in &self.requires.os {
            if !SUPPORTED_OS.contains(&os.as_str()) {
                problems.push(format!(
                    "requires.os: '{os}' is not a supported OS; use {}",
                    SUPPORTED_OS.join(" or ")
                ));
            }
        }
        for (prefix, requirement) in &self.requires.mcp {
            if let Some(version) = &requirement.version {
                problems.extend(semver_problem(
                    &format!("requires.mcp.{prefix}.version"),
                    version,
                ));
            }
        }
        problems
    }
}

impl LockEntry {
    pub fn problems(&self, name: &str) -> Vec<String> {
        let mut problems = Vec::new();
        let field = |key: &str| format!("packs.{name}.{key}");
        if self.git.trim().is_empty() {
            problems.push(format!("{}: must not be empty", field("git")));
        }
        let sha_ok =
            matches!(self.sha.len(), 40 | 64) && self.sha.chars().all(|c| c.is_ascii_hexdigit());
        if !sha_ok {
            problems.push(format!(
                "{}: '{}' is not a 40 or 64 character hex commit id",
                field("sha"),
                self.sha
            ));
        }
        let hash_ok = self.hash.strip_prefix(HASH_PREFIX).is_some_and(|hex| {
            hex.len() == 64 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        });
        if !hash_ok {
            problems.push(format!(
                "{}: '{}' must be {HASH_PREFIX} followed by 64 lowercase hex digits",
                field("hash"),
                self.hash
            ));
        }
        problems
    }
}

impl PackLock {
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (name, entry) in &self.packs {
            if let Err(e) = check_pack_name(name) {
                problems.push(format!("packs.{name}: {e}"));
            }
            problems.extend(entry.problems(name));
        }
        problems
    }
}

fn refuse_problems(what: &dyn fmt::Display, problems: Vec<String>) -> Result<(), String> {
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!("{what}: {}", problems.join("; ")))
}

fn read_manifest(text: &str, what: &dyn fmt::Display) -> Result<PackManifest, String> {
    let manifest: PackManifest = read_versioned(text, what, &MANIFEST_CONTRACT)?;
    refuse_problems(what, manifest.problems())?;
    Ok(manifest)
}

fn read_lock(text: &str, what: &dyn fmt::Display) -> Result<PackLock, String> {
    let lock: PackLock = read_versioned(text, what, &LOCK_CONTRACT)?;
    refuse_problems(what, lock.problems())?;
    Ok(lock)
}

pub fn parse_manifest(text: &str) -> Result<PackManifest, String> {
    read_manifest(text, &MANIFEST_FILE)
}

pub fn load_manifest(path: &Path) -> Result<PackManifest, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    read_manifest(&text, &path.display())
}

pub fn parse_lock(text: &str) -> Result<PackLock, String> {
    read_lock(text, &LOCK_FILE)
}

pub fn load_lock(path: &Path) -> Result<PackLock, String> {
    if !path.exists() {
        return Ok(PackLock::default());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    read_lock(&text, &path.display())
}

pub fn lock_to_string(lock: &PackLock) -> String {
    let stamped = PackLock {
        version: LOCK_FORMAT,
        packs: lock.packs.clone(),
    };
    toml::to_string(&stamped).expect("a pack lock always serializes")
}

pub fn write_lock(path: &Path, lock: &PackLock) -> Result<(), String> {
    let staged = path.with_file_name(format!("{LOCK_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&staged, lock_to_string(lock))
        .map_err(|e| format!("writing {}: {e}", staged.display()))?;
    std::fs::rename(&staged, path).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("writing {}: {e}", path.display())
    })
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
