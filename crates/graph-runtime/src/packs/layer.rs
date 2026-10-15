use super::content_hash;
use super::git;
use super::refs::{clone_moved, Target};
use anyhow::{Context, Result};
use graph_config::pack::{
    load_lock, load_manifest, write_lock, LayerKind, PackEntry, PackLock, MANIFEST_FILE, PACKS_DIR,
};
use graph_config::{CONFIG_FORMAT, FORMAT_KEY};
use graph_core::packs::{check_installed_name, load_pack, LoadedPack, PackSource};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone)]
pub struct Layer {
    pub kind: LayerKind,
    pub dir: PathBuf,
    pub flag: String,
    pub config_paths: Vec<PathBuf>,
}

pub struct LayerState {
    pub packs: BTreeMap<String, PackEntry>,
    pub mcp_servers: BTreeSet<String>,
    pub lock: PackLock,
}

impl Layer {
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(graph_config::pack::LOCK_FILE)
    }

    pub fn copy_dir(&self, name: &str) -> PathBuf {
        self.dir.join(PACKS_DIR).join(name)
    }

    pub fn command(&self, verb: &str, name: &str) -> String {
        format!("graph pack {verb} {name}{}", self.flag)
    }

    pub fn state(&self) -> Result<LayerState> {
        let loaded = graph_config::load_from(&self.config_paths, &graph_core::secrets::system())?;
        let packs = loaded
            .pack_layers
            .into_iter()
            .find(|layer| layer.dir == self.dir)
            .map(|layer| layer.packs)
            .unwrap_or_default();
        let lock = load_lock(&self.lock_path()).map_err(anyhow::Error::msg)?;
        Ok(LayerState {
            packs,
            mcp_servers: loaded.config.mcp.keys().cloned().collect(),
            lock,
        })
    }

    pub fn staging(&self) -> Result<TempDir> {
        tempfile::Builder::new()
            .prefix(".pack-staging-")
            .tempdir_in(&self.dir)
            .with_context(|| format!("creating a staging directory in {}", self.dir.display()))
    }

    pub fn read_config(&self) -> Result<String> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(format!("{FORMAT_KEY} = {CONFIG_FORMAT}\n"));
        }
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    }

    pub fn write_config(&self, text: &str) -> Result<()> {
        write_atomic(&self.config_path(), text)
    }

    pub fn write_lock(&self, lock: &PackLock) -> Result<()> {
        write_lock(&self.lock_path(), lock).map_err(anyhow::Error::msg)
    }
}

pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let staged = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, path).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        anyhow::anyhow!("writing {}: {e}", path.display())
    })
}

pub fn with_pack_entry(
    config: &str,
    name: &str,
    source: &str,
    path: &str,
    reference: &str,
) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = config.parse().context("parsing config.toml")?;
    if doc.get("packs").is_none() {
        let mut packs = toml_edit::Table::new();
        packs.set_implicit(true);
        doc.insert("packs", toml_edit::Item::Table(packs));
    }
    let mut entry = toml_edit::Table::new();
    entry.insert("git", toml_edit::value(source));
    if !path.is_empty() {
        entry.insert("path", toml_edit::value(path));
    }
    entry.insert("ref", toml_edit::value(reference));
    match doc.get_mut("packs") {
        Some(toml_edit::Item::Table(packs)) => {
            packs.insert(name, toml_edit::Item::Table(entry));
        }
        Some(toml_edit::Item::Value(toml_edit::Value::InlineTable(packs))) => {
            packs.insert(
                name,
                toml_edit::Value::InlineTable(entry.into_inline_table()),
            );
        }
        _ => anyhow::bail!("[packs] in config.toml is not a table"),
    }
    Ok(doc.to_string())
}

pub fn with_pack_ref(config: &str, name: &str, reference: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = config.parse().context("parsing config.toml")?;
    let entry = doc
        .get_mut("packs")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|packs| packs.get_mut(name))
        .and_then(toml_edit::Item::as_table_like_mut)
        .with_context(|| format!("config.toml has no [packs.{name}] table"))?;
    match entry.get_mut("ref").and_then(toml_edit::Item::as_value_mut) {
        Some(value) => {
            let decor = value.decor().clone();
            *value = reference.into();
            *value.decor_mut() = decor;
        }
        None => {
            entry.insert("ref", toml_edit::value(reference));
        }
    }
    Ok(doc.to_string())
}

pub fn without_pack_entry(config: &str, name: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = config.parse().context("parsing config.toml")?;
    let emptied = match doc
        .get_mut("packs")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        Some(packs) => {
            packs.remove(name);
            packs.is_empty()
        }
        None => false,
    };
    if emptied {
        doc.remove("packs");
    }
    Ok(doc.to_string())
}

pub struct Fetched {
    pub staging: TempDir,
    pub path: PathBuf,
    pub sha: String,
}

pub fn fetch(layer: &Layer, url: &str, target: &Target, dir: &str) -> Result<Fetched, String> {
    let reference = &target.reference;
    let staging = layer.staging().map_err(|e| format!("{e:#}"))?;
    let path = staging.path().join("new");
    let clone = if dir.is_empty() {
        path.clone()
    } else {
        staging.path().join("clone")
    };
    let sha = git::clone_at(url, reference, &clone)?;
    if let Some(moved) = clone_moved(target, url, &sha) {
        return Err(moved);
    }
    let root = clone.join(dir);
    if !root.is_dir() {
        return Err(format!("{url} at {reference} has no directory {dir}"));
    }
    if !root.join(MANIFEST_FILE).is_file() {
        let place = if dir.is_empty() {
            "at its root".to_string()
        } else {
            format!("in {dir}/")
        };
        return Err(format!(
            "{url} at {reference} has no {MANIFEST_FILE} {place}; is it a pack?"
        ));
    }
    if dir.is_empty() {
        std::fs::remove_dir_all(path.join(".git"))
            .map_err(|e| format!("removing .git from the staged copy: {e}"))?;
    } else {
        std::fs::rename(&root, &path).map_err(|e| format!("staging {dir}/ of the clone: {e}"))?;
        std::fs::remove_dir_all(&clone).map_err(|e| format!("removing the clone: {e}"))?;
    }
    Ok(Fetched { staging, path, sha })
}

impl Fetched {
    pub fn manifest_name(&self) -> Result<String, String> {
        load_manifest(&self.path.join(MANIFEST_FILE))
            .map(|manifest| manifest.name)
            .map_err(|problem| self.relative(problem))
    }

    pub fn verify(
        &self,
        name: &str,
        tag: Option<&str>,
        layer: &Layer,
        entry: Option<&PackEntry>,
        mcp_servers: &BTreeSet<String>,
    ) -> Result<(String, LoadedPack), Vec<String>> {
        check_installed_name(name, mcp_servers).map_err(|problem| vec![problem])?;
        let source = PackSource {
            name: name.to_string(),
            tag: tag.map(str::to_string),
            layer: layer.kind,
            dir: self.path.clone(),
        };
        let (include, exclude) = entry
            .map(|entry| (entry.include.as_slice(), entry.exclude.as_slice()))
            .unwrap_or_default();
        let loaded = load_pack(source, include, exclude).map_err(|problems| {
            problems
                .into_iter()
                .map(|problem| self.relative(problem))
                .collect::<Vec<_>>()
        })?;
        let hash =
            content_hash(&self.path).map_err(|e| vec![format!("hashing the staged pack: {e}")])?;
        Ok((hash, loaded))
    }

    fn relative(&self, problem: String) -> String {
        problem.replace(&format!("{}/", self.path.display()), "")
    }

    pub fn install(&self, copy: &Path) -> Result<Swap> {
        let previous = self.staging.path().join("old");
        if copy.exists() {
            std::fs::rename(copy, &previous)
                .with_context(|| format!("moving {} aside", copy.display()))?;
        } else if let Some(parent) = copy.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if let Err(error) = std::fs::rename(&self.path, copy) {
            if previous.exists() {
                let _ = std::fs::rename(&previous, copy);
            }
            return Err(anyhow::anyhow!(
                "moving the staged pack to {}: {error}",
                copy.display()
            ));
        }
        Ok(Swap {
            copy: copy.to_path_buf(),
            staged: self.path.clone(),
            previous: previous.exists().then_some(previous),
        })
    }
}

pub struct Swap {
    copy: PathBuf,
    staged: PathBuf,
    previous: Option<PathBuf>,
}

impl Swap {
    pub fn undo(self) {
        let _ = std::fs::rename(&self.copy, &self.staged);
        if let Some(previous) = self.previous {
            let _ = std::fs::rename(previous, &self.copy);
        }
    }
}

pub struct Aside {
    _staging: TempDir,
    path: PathBuf,
    copy: PathBuf,
}

impl Aside {
    pub fn restore(self) {
        let _ = std::fs::rename(&self.path, &self.copy);
    }
}

pub fn set_aside(layer: &Layer, copy: &Path) -> Result<Option<Aside>> {
    if !copy.exists() {
        return Ok(None);
    }
    let staging = layer.staging()?;
    let path = staging.path().join("old");
    std::fs::rename(copy, &path).with_context(|| format!("moving {} aside", copy.display()))?;
    Ok(Some(Aside {
        _staging: staging,
        path,
        copy: copy.to_path_buf(),
    }))
}

pub fn caller_dirs(layer: &Layer) -> Vec<PathBuf> {
    let declared = std::fs::read_to_string(layer.config_path())
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .unwrap_or_default();
    let paths = |section: &str| -> Vec<PathBuf> {
        declared
            .get(section)
            .and_then(|table| table.get("paths"))
            .and_then(toml::Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(|path| graph_config::expand_tilde(Path::new(path)))
                    .collect()
            })
            .unwrap_or_else(|| vec![layer.dir.join(section)])
    };
    let mut dirs = paths("plans");
    dirs.extend(paths("tools"));
    dirs.push(layer.dir.join("agents"));
    dirs
}

pub fn callers(dirs: &[PathBuf], pack: &str, skip: &Path) -> Vec<PathBuf> {
    let pattern = regex::Regex::new(&format!(r"(^|[^A-Za-z0-9_$]){}__", regex::escape(pack)))
        .expect("an escaped pack name is a valid pattern");
    let mut found = BTreeSet::new();
    for dir in dirs {
        let mut pending = vec![dir.clone()];
        while let Some(current) = pending.pop() {
            if current == skip {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for path in entries.filter_map(|entry| entry.ok().map(|e| e.path())) {
                if path.is_dir() {
                    pending.push(path);
                } else if matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                ) && std::fs::read_to_string(&path)
                    .is_ok_and(|text| pattern.is_match(&text))
                {
                    found.insert(path);
                }
            }
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_entries_are_added_and_removed_keeping_formatting_and_comments() {
        let config = "version = 4\n\n# my models\n[models.default]\nprovider = \"anthropic\"   # keep\nmodel = \"x\"\n";
        let added = with_pack_entry(config, "example", "github.com/o/r", "", "v2").unwrap();
        assert!(added.starts_with(config), "{added}");
        assert!(
            added.contains("[packs.example]\ngit = \"github.com/o/r\"\nref = \"v2\"\n"),
            "{added}"
        );
        let nested = with_pack_entry(config, "example", "github.com/o/r", "ex", "v2").unwrap();
        assert!(
            nested.contains(
                "[packs.example]\ngit = \"github.com/o/r\"\npath = \"ex\"\nref = \"v2\"\n"
            ),
            "{nested}"
        );
        assert!(!added.contains("[packs]\n"), "{added}");
        let removed = without_pack_entry(&added, "example").unwrap();
        assert_eq!(removed.trim_end(), config.trim_end());
    }

    #[test]
    fn a_pack_ref_is_rewritten_in_place_keeping_formatting_and_comments() {
        let config = "version = 4\n\n[packs.example]\ngit = \"x\"\nref = \"v1\"   # line\n\n[packs.other]\ngit = \"y\"\n";
        let moved = with_pack_ref(config, "example", "v3").unwrap();
        assert_eq!(moved, config.replace("\"v1\"", "\"v3\""));
        let added = with_pack_ref(config, "other", "v2").unwrap();
        assert!(
            added.ends_with("[packs.other]\ngit = \"y\"\nref = \"v2\"\n"),
            "{added}"
        );
        let inline =
            with_pack_ref("packs = { example = { git = \"x\" } }\n", "example", "v2").unwrap();
        assert!(inline.contains("ref = \"v2\""), "{inline}");
        assert!(with_pack_ref(config, "missing", "v2").is_err());
    }

    #[test]
    fn removing_one_entry_keeps_the_others() {
        let config = "[packs.a]\ngit = \"a\"\n\n[packs.b]\ngit = \"b\"\n";
        let removed = without_pack_entry(config, "a").unwrap();
        assert_eq!(removed.trim_start(), "[packs.b]\ngit = \"b\"\n");
    }

    #[test]
    fn callers_are_found_by_pack_prefix_only() {
        let dir = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "plans/uses.yaml",
            "tool_name: example__plan__describe_file\n",
        );
        write("plans/other.yaml", "tool_name: myexample__tool__x\n");
        write("tools/builtin/deep.yaml", "args: [\"example__tool__x\"]\n");
        write(
            "packs/example/plans/self.yaml",
            "tool_name: example__tool__x\n",
        );
        let found = callers(
            &[
                dir.path().join("plans"),
                dir.path().join("tools"),
                dir.path().join("packs"),
            ],
            "example",
            &dir.path().join("packs/example"),
        );
        assert_eq!(
            found,
            [
                dir.path().join("plans/uses.yaml"),
                dir.path().join("tools/builtin/deep.yaml")
            ]
        );
    }
}
