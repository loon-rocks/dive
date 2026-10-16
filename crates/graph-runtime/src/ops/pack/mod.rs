use crate::packs::git::{self, is_bare_name, remote_url, source_of};
use crate::packs::layer::{self, Layer, LayerState};
use crate::packs::refs::{self, PackRef, Target};
use crate::packs::{content_hash, RequirementReport};
use crate::runtime::Workspace;
use anyhow::{bail, Result};
use graph_config::pack::{check_pack_path, LayerKind, LockEntry, PackEntry, PackLock, PACKS_DIR};
use graph_core::packs::{check_installed_name, layer_label};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub mod major;

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("{0}")]
    AlreadyInstalled(String),
    #[error("{0}")]
    ConfiguredDifferently(String),
    #[error("{0}")]
    InvalidPath(String),
    #[error("{0}")]
    NameUnavailable(String),
    #[error("{0}")]
    TagMoved(String),
    #[error("{0}")]
    Unresolvable(String),
    #[error("{0}")]
    FetchFailed(String),
    #[error("{message}")]
    NotAPack {
        message: String,
        problems: Vec<String>,
    },
    #[error("{message}")]
    DoesNotLoad {
        message: String,
        problems: Vec<String>,
    },
    #[error("{0}")]
    NotInstalled(String),
    #[error("{0}")]
    NoTerminal(String),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

impl PackError {
    pub fn problems(&self) -> &[String] {
        match self {
            PackError::NotAPack { problems, .. } | PackError::DoesNotLoad { problems, .. } => {
                problems
            }
            _ => &[],
        }
    }

    pub fn is_refusal(&self) -> bool {
        !matches!(self, PackError::Failed(_))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayerChoice {
    pub global: bool,
    pub dir: Option<PathBuf>,
}

pub fn global_dir() -> PathBuf {
    let config = graph_config::expand_tilde(&graph_config::global_config_path());
    config.parent().map(Path::to_path_buf).unwrap_or_default()
}

pub fn project_dir(workspace: &Workspace) -> PathBuf {
    workspace.project_dir()
}

pub fn layer_at(kind: LayerKind, dir: PathBuf, flag: String) -> Layer {
    let global = graph_config::expand_tilde(&graph_config::global_config_path());
    let own = dir.join(layer::CONFIG_FILE);
    let config_paths = if kind == LayerKind::Global {
        vec![own]
    } else {
        vec![global, own]
    };
    Layer {
        kind,
        dir,
        flag,
        config_paths,
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn target(workspace: &Workspace, choice: &LayerChoice) -> Result<Layer> {
    if choice.global {
        let dir = global_dir();
        if !dir.is_dir() {
            bail!(
                "{} does not exist; create it with `graph config init --global`",
                dir.display()
            );
        }
        return Ok(layer_at(LayerKind::Global, dir, " --global".to_string()));
    }
    if let Some(dir) = &choice.dir {
        if !dir.is_dir() {
            bail!("{} is not a directory", dir.display());
        }
        let kind = if same_dir(dir, &global_dir()) {
            LayerKind::Global
        } else {
            LayerKind::Project
        };
        return Ok(layer_at(
            kind,
            dir.clone(),
            format!(" --dir {}", dir.display()),
        ));
    }
    project_layer(project_dir(workspace))
}

pub fn project_layer(dir: PathBuf) -> Result<Layer> {
    if !dir.is_dir() {
        bail!(
            "{} does not exist; run `graph config init` to create the project's graph directory, or pass --global or --dir",
            dir.display()
        );
    }
    Ok(layer_at(LayerKind::Project, dir, String::new()))
}

pub fn listed_layers(workspace: &Workspace, choice: &LayerChoice) -> Result<Vec<Layer>> {
    if choice.global || choice.dir.is_some() {
        return Ok(vec![target(workspace, choice)?]);
    }
    let mut layers = Vec::new();
    let global = global_dir();
    if global.is_dir() {
        layers.push(layer_at(LayerKind::Global, global, " --global".to_string()));
    }
    let project = project_dir(workspace);
    if project.is_dir() {
        layers.push(layer_at(LayerKind::Project, project, String::new()));
    }
    Ok(layers)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddRequest {
    pub source: String,
    pub as_name: Option<String>,
    pub reference: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackInstall {
    pub ok: bool,
    pub name: String,
    pub layer: String,
    pub git: String,
    pub path: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub tag: Option<String>,
    pub sha: String,
    pub hash: String,
    pub saved_to: String,
    pub warnings: Vec<String>,
    pub unmet: Vec<String>,
    pub mcp_snippets: Vec<String>,
    #[serde(skip)]
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CrossingRow {
    pub from: String,
    pub to: String,
    pub migration: Option<String>,
    pub files: Vec<Value>,
    pub guide: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrossedColumns {
    pub crossed: Vec<CrossingRow>,
    pub stopped_at: Option<String>,
    pub left: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackUpdate {
    pub name: String,
    pub status: String,
    pub summary: String,
    pub problems: Vec<String>,
    pub held_back: Option<String>,
    pub unmet: Vec<String>,
    pub mcp_snippets: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(flatten)]
    pub crossing: Option<CrossedColumns>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PackUpdates {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    pub packs: Vec<PackUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problems: Option<Vec<String>>,
    #[serde(skip)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackRemoved {
    pub ok: bool,
    pub name: String,
    pub layer: String,
    pub removed: String,
    pub callers: Vec<String>,
    #[serde(skip)]
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RemoteColumns {
    Unreachable {
        #[serde(rename = "remoteError")]
        remote_error: String,
    },
    Reached {
        #[serde(rename = "tagMoved")]
        tag_moved: Option<String>,
        newest: Option<String>,
        #[serde(rename = "newestInRef")]
        newest_in_ref: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackRow {
    pub name: String,
    pub layer: String,
    pub dir: String,
    pub git: String,
    pub path: String,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub installed: bool,
    pub tag: Option<String>,
    pub sha: Option<String>,
    pub modified: Option<bool>,
    pub shadowed: bool,
    pub shadowed_by: Option<String>,
    #[serde(flatten)]
    pub remote: Option<RemoteColumns>,
}

impl PackRow {
    pub fn line(&self, offline: bool) -> String {
        let dash = |value: Option<&String>| value.cloned().unwrap_or_else(|| "-".to_string());
        let installed = if self.installed {
            self.tag
                .clone()
                .or_else(|| self.sha.as_deref().map(short))
                .unwrap_or_else(|| "-".to_string())
        } else {
            "not installed".to_string()
        };
        let mut columns = vec![self.name.clone(), self.layer.clone(), installed];
        let mut tag_moved = false;
        if !offline {
            match &self.remote {
                Some(RemoteColumns::Unreachable { .. }) => {
                    columns.push("remote unreachable".to_string())
                }
                Some(RemoteColumns::Reached {
                    tag_moved: moved,
                    newest,
                    newest_in_ref,
                }) => {
                    tag_moved = moved.is_some();
                    columns.push(format!(
                        "ref {}: {}",
                        dash(self.reference.as_ref()),
                        dash(newest_in_ref.as_ref())
                    ));
                    columns.push(format!("newest: {}", dash(newest.as_ref())));
                }
                None => {
                    columns.push(format!("ref {}: -", dash(self.reference.as_ref())));
                    columns.push("newest: -".to_string());
                }
            }
        }
        let mut flags = Vec::new();
        if self.modified == Some(true) {
            flags.push("local edits".to_string());
        }
        if let Some(by) = &self.shadowed_by {
            flags.push(format!("shadowed by the {by} layer"));
        }
        if tag_moved {
            flags.push("release tag moved since install; graph won't install it".to_string());
        }
        if !flags.is_empty() {
            columns.push(flags.join(", "));
        }
        format!("{}\n", columns.join("\t"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackList {
    pub packs: Vec<PackRow>,
    pub count: usize,
    pub offline: bool,
}

impl PackList {
    pub fn text(&self) -> String {
        self.packs
            .iter()
            .map(|row| row.line(self.offline))
            .collect()
    }
}

struct Install {
    name: Option<String>,
    source: String,
    path: String,
    reference: Option<String>,
    entry: Option<PackEntry>,
}

pub fn add(layer: &Layer, request: &AddRequest) -> Result<PackInstall, PackError> {
    let state = layer.state()?;
    let configured = state.packs.get(&request.source).filter(|_| {
        is_bare_name(&request.source)
            && request
                .as_name
                .as_ref()
                .is_none_or(|name| *name == request.source)
    });
    let install = match configured {
        Some(entry) => {
            let name = request.source.clone();
            if layer.copy_dir(&name).exists() {
                return Err(PackError::AlreadyInstalled(format!(
                    "pack '{name}' is already installed at {}; move it with `{}`",
                    layer.copy_dir(&name).display(),
                    layer.command("update", &name)
                )));
            }
            if let Some(wanted) = &request.reference {
                if Some(wanted) != entry.reference.as_ref() {
                    return Err(PackError::ConfiguredDifferently(format!(
                        "pack '{name}' is configured in {} with ref {}; edit [packs.{name}] there to change it, then run `{}`",
                        layer.config_path().display(),
                        entry.reference.as_deref().unwrap_or("(none)"),
                        layer.command("add", &name)
                    )));
                }
            }
            if let Some(wanted) = &request.path {
                if *wanted != entry.path {
                    return Err(PackError::ConfiguredDifferently(format!(
                        "pack '{name}' is configured in {} with path '{}'; edit [packs.{name}] there to change it, then run `{}`",
                        layer.config_path().display(),
                        entry.path,
                        layer.command("add", &name)
                    )));
                }
            }
            Install {
                name: Some(name),
                source: entry.git.clone(),
                path: entry.path.clone(),
                reference: entry.reference.clone(),
                entry: Some(entry.clone()),
            }
        }
        None => {
            let (source, path) = source_of(&request.source, request.path.as_deref());
            Install {
                name: request.as_name.clone(),
                source,
                path,
                reference: request.reference.clone(),
                entry: None,
            }
        }
    };
    if let Err(problem) = check_pack_path(&install.path) {
        return Err(PackError::InvalidPath(format!("path {problem}")));
    }
    if let (None, Some(name)) = (&install.entry, &install.name) {
        name_available(layer, &state, name)?;
    }

    let url = remote_url(&install.source);
    let at = refs::pack_at(&url, &install.path);
    let remote = git::ls_remote(&url).map_err(anyhow::Error::msg)?;
    if let Some(moved) = state
        .lock
        .packs
        .values()
        .find_map(|locked| refs::moved_tag(&remote, &install.source, &install.path, locked))
    {
        return Err(PackError::TagMoved(moved));
    }
    let mut warnings = Vec::new();
    let (target, reference) = add_target(
        &remote,
        install.reference.as_deref(),
        &url,
        &install.path,
        &mut warnings,
    )
    .map_err(PackError::Unresolvable)?;

    let fetched =
        layer::fetch(layer, &url, &target, &install.path).map_err(PackError::FetchFailed)?;
    let name = match install.name {
        Some(name) => name,
        None => fetched
            .manifest_name()
            .map_err(|problem| PackError::NotAPack {
                message: format!("{at} at {} is not a pack", target.reference),
                problems: vec![problem],
            })?,
    };
    if install.entry.is_none() && request.as_name.is_none() {
        name_available(layer, &state, &name)?;
    }
    let release = refs::label(&install.path, target.tag.as_deref());
    let (hash, loaded) = fetched
        .verify(
            &name,
            release.as_deref(),
            layer,
            install.entry.as_ref(),
            &state.mcp_servers,
        )
        .map_err(|problems| PackError::DoesNotLoad {
            message: format!(
                "pack '{name}' does not load from {at} at {}",
                target.reference
            ),
            problems,
        })?;

    let copy = layer.copy_dir(&name);
    let swap = fetched.install(&copy)?;
    let config_before = layer.read_config()?;
    let config_existed = layer.config_path().exists();
    if install.entry.is_none() {
        let written = layer::with_pack_entry(
            &config_before,
            &name,
            &install.source,
            &install.path,
            &reference,
        )
        .and_then(|text| layer.write_config(&text));
        if let Err(error) = written {
            swap.undo();
            return Err(error.into());
        }
    }
    let locked = LockEntry {
        git: install.source.clone(),
        path: install.path.clone(),
        tag: target.tag.clone(),
        sha: fetched.sha.clone(),
        hash: hash.clone(),
    };
    let mut lock = state.lock;
    lock.packs.insert(name.clone(), locked);
    if let Err(error) = layer.write_lock(&lock) {
        if install.entry.is_none() {
            if config_existed {
                let _ = layer.write_config(&config_before);
            } else {
                let _ = std::fs::remove_file(layer.config_path());
            }
        }
        swap.undo();
        return Err(error.into());
    }

    let installed = release.unwrap_or_else(|| short(&fetched.sha));
    let mut lines = vec![format!(
        "installed {name} {installed} from {} ({} layer, ref {reference})",
        refs::pack_at(&install.source, &install.path),
        layer_label(layer.kind)
    )];
    lines.extend(warnings.iter().cloned());
    let requirements = RequirementReport::of(&loaded, &state.mcp_servers);
    lines.extend(requirements.lines(&name));
    Ok(PackInstall {
        ok: true,
        layer: layer_label(layer.kind).to_string(),
        git: install.source,
        path: install.path,
        reference,
        tag: target.tag,
        sha: fetched.sha.clone(),
        hash,
        saved_to: copy.display().to_string(),
        warnings,
        unmet: requirements.unmet,
        mcp_snippets: requirements.snippets,
        lines,
        name,
    })
}

fn add_target(
    remote: &git::RemoteRefs,
    reference: Option<&str>,
    url: &str,
    path: &str,
    warnings: &mut Vec<String>,
) -> Result<(Target, String), String> {
    if let Some(text) = reference {
        let target = refs::resolve(remote, &PackRef::parse(text), url, path)?;
        if target.tag.is_none() {
            warnings.push(branch_warning(text));
        }
        return Ok((target, text.to_string()));
    }
    if let Some(release) = refs::newest(&refs::releases(remote, path)) {
        return Ok((release.target(), format!("v{}", release.version.major)));
    }
    let at = refs::pack_at(url, path);
    let tags = format!("{}vX.Y.Z", refs::tag_prefix(path));
    let branch = remote
        .default_branch
        .clone()
        .ok_or_else(|| format!("{at} has no {tags} release tags and no default branch"))?;
    warnings.push(format!(
        "{at} has no {tags} release tags; installing the head of its default branch '{branch}'"
    ));
    warnings.push(branch_warning(&branch));
    let target = refs::resolve(remote, &PackRef::Branch(branch.clone()), url, path)?;
    Ok((target, branch))
}

fn name_available(layer: &Layer, state: &LayerState, name: &str) -> Result<(), PackError> {
    if let Err(problem) = check_installed_name(name, &state.mcp_servers) {
        return Err(PackError::NameUnavailable(problem));
    }
    if state.packs.contains_key(name) {
        return Err(PackError::NameUnavailable(format!(
            "a pack named '{name}' is already configured in {}; install this one under another name with --as, or remove it first with `{}`",
            layer.config_path().display(),
            layer.command("remove", name)
        )));
    }
    if layer.copy_dir(name).exists() {
        return Err(PackError::NameUnavailable(format!(
            "{} already exists but is not configured; delete it or install under another name with --as",
            layer.copy_dir(name).display()
        )));
    }
    Ok(())
}

fn branch_warning(branch: &str) -> String {
    format!("tracking branch '{branch}' rather than a release; `graph pack update` moves it to the branch head")
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn installed_version(locked: &LockEntry) -> String {
    refs::label(&locked.path, locked.tag.as_deref()).unwrap_or_else(|| short(&locked.sha))
}

fn wanted_ref(entry: &PackEntry, lock: &LockEntry) -> Option<PackRef> {
    match &entry.reference {
        Some(text) => Some(PackRef::parse(text)),
        None => lock
            .tag
            .as_deref()
            .and_then(|tag| refs::release_version(&lock.path, tag))
            .map(|version| PackRef::Major(version.major)),
    }
}

enum Update {
    Moved {
        from: String,
        to: String,
        requirements: RequirementReport,
    },
    Crossed(major::Crossed),
    Current(String),
    Pinned(String),
    Branch(String),
    Refused(String, Vec<String>),
}

fn update_names(
    layer: &Layer,
    state: &LayerState,
    names: &[String],
) -> Result<Vec<String>, Box<PackUpdates>> {
    let names: Vec<String> = if names.is_empty() {
        state.packs.keys().cloned().collect()
    } else {
        names.to_vec()
    };
    if names.is_empty() {
        return Err(Box::new(PackUpdates {
            ok: true,
            layer: None,
            packs: Vec::new(),
            error: None,
            problems: None,
            note: format!("no packs configured in {}", layer.config_path().display()),
        }));
    }
    Ok(names)
}

fn not_configured(layer: &Layer, name: &str) -> Update {
    Update::Refused(
        format!(
            "not configured in {}; install it with `{}`",
            layer.config_path().display(),
            layer.command("add", name)
        ),
        Vec::new(),
    )
}

pub fn update(layer: &Layer, names: &[String], force: bool) -> Result<PackUpdates> {
    let state = layer.state()?;
    let names = match update_names(layer, &state, names) {
        Ok(names) => names,
        Err(empty) => return Ok(*empty),
    };
    let mut lock = state.lock.clone();
    let mut results = Vec::new();
    for name in names {
        let mut held_back = None;
        let result = match state.packs.get(&name) {
            None => not_configured(layer, &name),
            Some(entry) => update_one(
                layer,
                &state,
                entry,
                &name,
                force,
                &mut lock,
                &mut held_back,
            )?,
        };
        results.push((name, result, held_back));
    }
    Ok(update_outcome(layer, results))
}

fn update_outcome(layer: &Layer, updates: Vec<(String, Update, Option<String>)>) -> PackUpdates {
    let count = updates.len();
    let mut results = Vec::new();
    let mut lines = Vec::new();
    let mut refusals = 0;
    for (name, result, held_back) in updates {
        let name = name.as_str();
        let (status, line, problems) = match &result {
            Update::Moved { from, to, .. } if from == to => (
                "reinstalled",
                format!("{name}: reinstalled {to}, discarding local edits"),
                Vec::new(),
            ),
            Update::Moved { from, to, .. } => {
                ("updated", format!("{name}: {from} -> {to}"), Vec::new())
            }
            Update::Crossed(crossed) => {
                let status = crossed.status();
                if status == "refused" {
                    refusals += 1;
                }
                (status, crossed.summary(name, layer), crossed.problems())
            }
            Update::Current(at) => ("current", format!("{name}: up to date at {at}"), Vec::new()),
            Update::Pinned(tag) => (
                "pinned",
                format!("{name}: pinned to {tag} by its ref; skipped"),
                Vec::new(),
            ),
            Update::Branch(branch) => (
                "branch",
                format!(
                    "{name}: tracks branch '{branch}', which has no majors to cross; `{}` moves it to the branch head",
                    layer.command("update", name)
                ),
                Vec::new(),
            ),
            Update::Refused(reason, problems) => {
                refusals += 1;
                ("refused", format!("{name}: {reason}"), problems.clone())
            }
        };
        lines.push(line.clone());
        if let Update::Crossed(crossed) = &result {
            lines.extend(crossed.lines());
        }
        lines.extend(problems.iter().map(|problem| format!("  {problem}")));
        if let Some(note) = &held_back {
            lines.push(note.clone());
        }
        let requirements = match &result {
            Update::Moved { requirements, .. } => requirements.clone(),
            Update::Crossed(crossed) => crossed.requirements.clone(),
            _ => RequirementReport::default(),
        };
        lines.extend(requirements.lines(name));
        let (from, to) = match &result {
            Update::Moved { from, to, .. } => (Some(from.clone()), Some(to.clone())),
            Update::Crossed(crossed) => (Some(crossed.from.clone()), crossed.to()),
            _ => (None, None),
        };
        results.push(PackUpdate {
            name: name.to_string(),
            status: status.to_string(),
            summary: line,
            problems,
            held_back,
            unmet: requirements.unmet,
            mcp_snippets: requirements.snippets,
            from,
            to,
            crossing: match &result {
                Update::Crossed(crossed) => Some(crossed.columns()),
                _ => None,
            },
        });
    }
    let ok = refusals == 0;
    PackUpdates {
        ok,
        layer: Some(layer_label(layer.kind).to_string()),
        packs: results,
        error: (!ok).then(|| format!("{refusals} of {count} packs not updated")),
        problems: (!ok).then(|| lines.clone()),
        note: lines.join("\n  "),
    }
}

fn local_edits(
    layer: &Layer,
    name: &str,
    copy: &Path,
    locked: &LockEntry,
    force: bool,
) -> Result<Result<bool, Update>> {
    let modified = content_hash(copy)? != locked.hash;
    if modified && !force {
        return Ok(Err(Update::Refused(
            format!(
                "{} has local edits (its content no longer matches {}); keep them by leaving the pack alone, or overwrite them with `{} --force`",
                copy.display(),
                layer.lock_path().display(),
                layer.command("update", name)
            ),
            Vec::new(),
        )));
    }
    Ok(Ok(modified))
}

fn update_one(
    layer: &Layer,
    state: &LayerState,
    entry: &PackEntry,
    name: &str,
    force: bool,
    lock: &mut PackLock,
    held_back: &mut Option<String>,
) -> Result<Update> {
    let copy = layer.copy_dir(name);
    let Some(locked) = lock.packs.get(name).cloned().filter(|_| copy.is_dir()) else {
        return Ok(Update::Refused(
            format!("not installed; run `{}`", layer.command("add", name)),
            Vec::new(),
        ));
    };
    let modified = match local_edits(layer, name, &copy, &locked, force)? {
        Ok(modified) => modified,
        Err(refusal) => return Ok(refusal),
    };
    let wanted = wanted_ref(entry, &locked);
    if let Some(PackRef::Tag(version)) = &wanted {
        if !modified {
            return Ok(Update::Pinned(format!("v{version}")));
        }
    }
    let url = remote_url(&entry.git);
    let remote = match git::ls_remote(&url) {
        Ok(remote) => remote,
        Err(problem) => return Ok(Update::Refused(problem, Vec::new())),
    };
    if let Some(moved) = refs::moved_tag(&remote, &entry.git, &entry.path, &locked) {
        return Ok(Update::Refused(moved, Vec::new()));
    }
    let wanted = match wanted {
        Some(wanted) => wanted,
        None => match &remote.default_branch {
            Some(branch) => PackRef::Branch(branch.clone()),
            None => {
                return Ok(Update::Refused(
                    format!("{url} has no default branch to follow"),
                    Vec::new(),
                ))
            }
        },
    };
    let releases = refs::releases(&remote, &entry.path);
    if let (Some(line), Some(newest)) = (wanted.major_line(), refs::newest(&releases)) {
        if newest.version.major > line {
            *held_back = Some(format!(
                "{} available: {} --major",
                newest.label(),
                layer.command("update", name)
            ));
        }
    }
    let target = match refs::resolve(&remote, &wanted, &url, &entry.path) {
        Ok(target) => target,
        Err(problem) => return Ok(Update::Refused(problem, Vec::new())),
    };
    let from = installed_version(&locked);
    let release = refs::label(&entry.path, target.tag.as_deref());
    let to = release.clone().unwrap_or_else(|| short(&target.sha));
    let unchanged = target.tag == locked.tag && entry.path == locked.path && !modified;
    if unchanged && target.sha == locked.sha {
        return Ok(Update::Current(from));
    }
    let fetched = match layer::fetch(layer, &url, &target, &entry.path) {
        Ok(fetched) => fetched,
        Err(problem) => return Ok(Update::Refused(problem, Vec::new())),
    };
    if unchanged && content_hash(&fetched.path)? == locked.hash {
        return Ok(Update::Current(from));
    }
    let (hash, loaded) = match fetched.verify(
        name,
        release.as_deref(),
        layer,
        Some(entry),
        &state.mcp_servers,
    ) {
        Ok(hash) => hash,
        Err(problems) => {
            return Ok(Update::Refused(
                format!("{to} does not load; the installed {from} is unchanged"),
                problems,
            ))
        }
    };
    let swap = fetched.install(&copy)?;
    let previous = lock.packs.insert(
        name.to_string(),
        LockEntry {
            git: entry.git.clone(),
            path: entry.path.clone(),
            tag: target.tag.clone(),
            sha: fetched.sha.clone(),
            hash,
        },
    );
    if let Err(error) = layer.write_lock(lock) {
        if let Some(previous) = previous {
            lock.packs.insert(name.to_string(), previous);
        }
        swap.undo();
        return Err(error);
    }
    Ok(Update::Moved {
        from,
        to,
        requirements: RequirementReport::of(&loaded, &state.mcp_servers),
    })
}

pub fn remove(layer: &Layer, name: &str) -> Result<PackRemoved, PackError> {
    let state = layer.state()?;
    let copy = layer.copy_dir(name);
    let configured = state.packs.contains_key(name);
    let locked = state.lock.packs.contains_key(name);
    if !configured && !locked && !copy.exists() {
        return Err(PackError::NotInstalled(format!(
            "pack '{name}' is not installed in {}",
            layer.dir.display()
        )));
    }
    let aside = layer::set_aside(layer, &copy)?;
    let config_before = layer.read_config()?;
    if configured {
        let written = layer::without_pack_entry(&config_before, name)
            .and_then(|text| layer.write_config(&text));
        if let Err(error) = written {
            if let Some(aside) = aside {
                aside.restore();
            }
            return Err(error.into());
        }
    }
    if locked {
        let mut lock = state.lock.clone();
        lock.packs.remove(name);
        if let Err(error) = layer.write_lock(&lock) {
            if configured {
                let _ = layer.write_config(&config_before);
            }
            if let Some(aside) = aside {
                aside.restore();
            }
            return Err(error.into());
        }
    }
    drop(aside);

    let callers = layer::callers(&layer::caller_dirs(layer), name, &layer.dir.join(PACKS_DIR));
    let mut lines = vec![format!(
        "removed {name} from the {} layer ({})",
        layer_label(layer.kind),
        layer.dir.display()
    )];
    if !callers.is_empty() {
        lines.push(format!("these files still call {name}__ names:"));
        lines.extend(callers.iter().map(|path| format!("  {}", path.display())));
    }
    Ok(PackRemoved {
        ok: true,
        name: name.to_string(),
        layer: layer_label(layer.kind).to_string(),
        removed: copy.display().to_string(),
        callers: callers
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        lines,
    })
}

pub fn list(layers: &[Layer], offline: bool) -> Result<PackList> {
    let states = layers
        .iter()
        .map(|layer| layer.state().map(|state| (layer, state)))
        .collect::<Result<Vec<_>>>()?;
    let mut rows = Vec::new();
    for (index, (layer, state)) in states.iter().enumerate() {
        for (name, entry) in &state.packs {
            let copy = layer.copy_dir(name);
            let locked = state.lock.packs.get(name);
            let installed = copy.is_dir();
            let modified = match (installed, locked) {
                (true, Some(locked)) => Some(content_hash(&copy)? != locked.hash),
                _ => None,
            };
            let shadowed_by = states[index + 1..]
                .iter()
                .rev()
                .find(|(_, later)| later.packs.contains_key(name))
                .map(|(later, _)| layer_label(later.kind).to_string());
            rows.push(PackRow {
                name: name.clone(),
                layer: layer_label(layer.kind).to_string(),
                dir: copy.display().to_string(),
                git: entry.git.clone(),
                path: entry.path.clone(),
                reference: entry.reference.clone(),
                installed,
                tag: locked.and_then(|locked| refs::label(&locked.path, locked.tag.as_deref())),
                sha: locked.map(|locked| locked.sha.clone()),
                modified,
                shadowed: shadowed_by.is_some(),
                shadowed_by,
                remote: (!offline).then(|| remote_columns(entry, locked)),
            });
        }
    }
    Ok(PackList {
        count: rows.len(),
        packs: rows,
        offline,
    })
}

fn remote_columns(entry: &PackEntry, locked: Option<&LockEntry>) -> RemoteColumns {
    let url = remote_url(&entry.git);
    let remote = match git::ls_remote(&url) {
        Ok(remote) => remote,
        Err(problem) => {
            return RemoteColumns::Unreachable {
                remote_error: problem,
            }
        }
    };
    let tag_moved =
        locked.and_then(|locked| refs::moved_tag(&remote, &entry.git, &entry.path, locked));
    let releases = refs::releases(&remote, &entry.path);
    let newest = refs::newest(&releases).map(refs::Release::label);
    let wanted = match locked {
        Some(locked) => wanted_ref(entry, locked),
        None => entry.reference.as_deref().map(PackRef::parse),
    };
    let newest_in_ref = match wanted {
        Some(PackRef::Branch(branch)) => remote
            .heads
            .get(&branch)
            .map(|sha| format!("{branch}@{}", short(sha))),
        Some(wanted) => wanted
            .major_line()
            .and_then(|major| refs::newest_in(&releases, major))
            .map(refs::Release::label),
        None => None,
    };
    RemoteColumns::Reached {
        tag_moved,
        newest,
        newest_in_ref,
    }
}

#[cfg(test)]
mod tests;
