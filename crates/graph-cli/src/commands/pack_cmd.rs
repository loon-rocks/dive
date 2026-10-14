use crate::cli::{LayerArgs, PackCommand};
use crate::commands::outcome::{report, Outcome};
use crate::packs::git::{self, is_bare_name, remote_url, source_of};
use crate::packs::layer::{self, Layer};
use crate::packs::refs::{self, PackRef, Target};
use crate::packs::{content_hash, RequirementReport};
use anyhow::{bail, Result};
use graph_config::pack::{LayerKind, LockEntry, PackEntry, PACKS_DIR};
use graph_core::packs::{check_installed_name, layer_label};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn run(command: PackCommand) -> Result<()> {
    match command {
        PackCommand::Add {
            source,
            as_name,
            reference,
            layer,
            json,
        } => {
            let request = AddRequest {
                source,
                as_name,
                reference,
            };
            report(add(&target(&layer)?, &request)?, json)
        }
        PackCommand::List {
            layer,
            offline,
            json,
        } => report(list(&listed_layers(&layer)?, offline)?, json),
        PackCommand::Update {
            names,
            force,
            layer,
            json,
        } => report(update(&target(&layer)?, &names, force)?, json),
        PackCommand::Remove { name, layer, json } => report(remove(&target(&layer)?, &name)?, json),
    }
}

fn global_dir() -> PathBuf {
    let config = graph_config::expand_tilde(&graph_config::global_config_path());
    config.parent().map(Path::to_path_buf).unwrap_or_default()
}

fn project_dir() -> PathBuf {
    graph_config::project_config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn layer_at(kind: LayerKind, dir: PathBuf, flag: String) -> Layer {
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

fn target(args: &LayerArgs) -> Result<Layer> {
    if args.global {
        let dir = global_dir();
        if !dir.is_dir() {
            bail!(
                "{} does not exist; create it with `graph config init --global`",
                dir.display()
            );
        }
        return Ok(layer_at(LayerKind::Global, dir, " --global".to_string()));
    }
    if let Some(dir) = &args.dir {
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
    project_layer(project_dir())
}

fn project_layer(dir: PathBuf) -> Result<Layer> {
    if !dir.is_dir() {
        bail!(
            "{} does not exist; run `graph config init` to create the project's graph directory, or pass --global or --dir",
            dir.display()
        );
    }
    Ok(layer_at(LayerKind::Project, dir, String::new()))
}

fn listed_layers(args: &LayerArgs) -> Result<Vec<Layer>> {
    if args.global || args.dir.is_some() {
        return Ok(vec![target(args)?]);
    }
    let mut layers = Vec::new();
    let global = global_dir();
    if global.is_dir() {
        layers.push(layer_at(LayerKind::Global, global, " --global".to_string()));
    }
    let project = project_dir();
    if project.is_dir() {
        layers.push(layer_at(LayerKind::Project, project, String::new()));
    }
    Ok(layers)
}

fn refused(error: impl Into<String>, problems: Vec<String>) -> Outcome {
    Outcome::rejected(json!({
        "ok": false,
        "error": error.into(),
        "problems": problems,
    }))
}

fn refusal(error: impl Into<String>) -> Outcome {
    refused(error, Vec::new())
}

pub struct AddRequest {
    pub source: String,
    pub as_name: Option<String>,
    pub reference: Option<String>,
}

struct Install {
    name: Option<String>,
    source: String,
    reference: Option<String>,
    entry: Option<PackEntry>,
}

pub fn add(layer: &Layer, request: &AddRequest) -> Result<Outcome> {
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
                return Ok(refusal(format!(
                    "pack '{name}' is already installed at {}; move it with `{}`",
                    layer.copy_dir(&name).display(),
                    layer.command("update", &name)
                )));
            }
            if let Some(wanted) = &request.reference {
                if Some(wanted) != entry.reference.as_ref() {
                    return Ok(refusal(format!(
                            "pack '{name}' is configured in {} with ref {}; edit [packs.{name}] there to change it, then run `{}`",
                            layer.config_path().display(),
                            entry.reference.as_deref().unwrap_or("(none)"),
                            layer.command("add", &name)
                        )));
                }
            }
            Install {
                name: Some(name),
                source: entry.git.clone(),
                reference: entry.reference.clone(),
                entry: Some(entry.clone()),
            }
        }
        None => Install {
            name: request.as_name.clone(),
            source: source_of(&request.source),
            reference: request.reference.clone(),
            entry: None,
        },
    };
    if let (None, Some(name)) = (&install.entry, &install.name) {
        if let Some(outcome) = name_refusal(layer, &state, name) {
            return Ok(outcome);
        }
    }

    let url = remote_url(&install.source);
    let remote = git::ls_remote(&url).map_err(anyhow::Error::msg)?;
    let mut warnings = Vec::new();
    let (target, reference) =
        match add_target(&remote, install.reference.as_deref(), &url, &mut warnings) {
            Ok(resolved) => resolved,
            Err(problem) => return Ok(refusal(problem)),
        };

    let fetched = layer::fetch(layer, &url, &target.reference).map_err(anyhow::Error::msg)?;
    let name = match install.name {
        Some(name) => name,
        None => match fetched.manifest_name() {
            Ok(name) => name,
            Err(problem) => {
                return Ok(refused(
                    format!("{url} at {} is not a pack", target.reference),
                    vec![problem],
                ))
            }
        },
    };
    if install.entry.is_none() && request.as_name.is_none() {
        if let Some(outcome) = name_refusal(layer, &state, &name) {
            return Ok(outcome);
        }
    }
    let (hash, loaded) = match fetched.verify(
        &name,
        target.tag.as_deref(),
        layer,
        install.entry.as_ref(),
        &state.mcp_servers,
    ) {
        Ok(verified) => verified,
        Err(problems) => {
            return Ok(refused(
                format!(
                    "pack '{name}' does not load from {url} at {}",
                    target.reference
                ),
                problems,
            ))
        }
    };

    let copy = layer.copy_dir(&name);
    let swap = fetched.install(&copy)?;
    let config_before = layer.read_config()?;
    let config_existed = layer.config_path().exists();
    if install.entry.is_none() {
        let written = layer::with_pack_entry(&config_before, &name, &install.source, &reference)
            .and_then(|text| layer.write_config(&text));
        if let Err(error) = written {
            swap.undo();
            return Err(error);
        }
    }
    let locked = LockEntry {
        git: install.source.clone(),
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
        return Err(error);
    }

    let installed = target.tag.clone().unwrap_or_else(|| short(&fetched.sha));
    let mut lines = vec![format!(
        "installed {name} {installed} from {} ({} layer, ref {reference})",
        install.source,
        layer_label(layer.kind)
    )];
    lines.extend(warnings.iter().cloned());
    let requirements = RequirementReport::of(&loaded, &state.mcp_servers);
    lines.extend(requirements.lines(&name));
    Ok(Outcome::ok(json!({
        "ok": true,
        "name": name,
        "layer": layer_label(layer.kind),
        "git": install.source,
        "ref": reference,
        "tag": target.tag,
        "sha": fetched.sha,
        "hash": hash,
        "savedTo": copy.display().to_string(),
        "warnings": warnings,
        "unmet": requirements.unmet,
        "mcpSnippets": requirements.snippets,
    }))
    .with_note(lines.join("\n  ")))
}

fn add_target(
    remote: &git::RemoteRefs,
    reference: Option<&str>,
    url: &str,
    warnings: &mut Vec<String>,
) -> Result<(Target, String), String> {
    if let Some(text) = reference {
        let target = refs::resolve(remote, &PackRef::parse(text), url)?;
        if target.tag.is_none() {
            warnings.push(branch_warning(text));
        }
        return Ok((target, text.to_string()));
    }
    if let Some(release) = refs::newest(&refs::releases(remote)) {
        return Ok((release.target(), format!("v{}", release.version.major)));
    }
    let branch = remote
        .default_branch
        .clone()
        .ok_or_else(|| format!("{url} has no vX.Y.Z release tags and no default branch"))?;
    warnings.push(format!(
        "{url} has no vX.Y.Z release tags; installing the head of its default branch '{branch}'"
    ));
    warnings.push(branch_warning(&branch));
    let target = refs::resolve(remote, &PackRef::Branch(branch.clone()), url)?;
    Ok((target, branch))
}

fn name_refusal(layer: &Layer, state: &layer::LayerState, name: &str) -> Option<Outcome> {
    if let Err(problem) = check_installed_name(name, &state.mcp_servers) {
        return Some(refusal(problem));
    }
    if state.packs.contains_key(name) {
        return Some(refusal(format!(
                "a pack named '{name}' is already configured in {}; install this one under another name with --as, or remove it first with `{}`",
                layer.config_path().display(),
                layer.command("remove", name)
            )));
    }
    if layer.copy_dir(name).exists() {
        return Some(refusal(format!(
                "{} already exists but is not configured; delete it or install under another name with --as",
                layer.copy_dir(name).display()
            )));
    }
    None
}

fn branch_warning(branch: &str) -> String {
    format!("tracking branch '{branch}' rather than a release; `graph pack update` moves it to the branch head")
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn wanted_ref(entry: &PackEntry, lock: &LockEntry) -> Option<PackRef> {
    match &entry.reference {
        Some(text) => Some(PackRef::parse(text)),
        None => lock
            .tag
            .as_deref()
            .and_then(refs::release_version)
            .map(|version| PackRef::Major(version.major)),
    }
}

enum Update {
    Moved {
        from: String,
        to: String,
        requirements: RequirementReport,
    },
    Current(String),
    Pinned(String),
    Refused(String, Vec<String>),
}

pub fn update(layer: &Layer, names: &[String], force: bool) -> Result<Outcome> {
    let state = layer.state()?;
    let names: Vec<String> = if names.is_empty() {
        state.packs.keys().cloned().collect()
    } else {
        names.to_vec()
    };
    if names.is_empty() {
        return Ok(
            Outcome::ok(json!({"ok": true, "packs": []})).with_note(format!(
                "no packs configured in {}",
                layer.config_path().display()
            )),
        );
    }
    let mut lock = state.lock.clone();
    let mut results = Vec::new();
    let mut lines = Vec::new();
    let mut refusals = 0;
    for name in &names {
        let mut held_back = None;
        let result = match state.packs.get(name) {
            None => Update::Refused(
                format!(
                    "not configured in {}; install it with `{}`",
                    layer.config_path().display(),
                    layer.command("add", name)
                ),
                Vec::new(),
            ),
            Some(entry) => {
                update_one(layer, &state, entry, name, force, &mut lock, &mut held_back)?
            }
        };
        let (status, line, problems) = match &result {
            Update::Moved { from, to, .. } if from == to => (
                "reinstalled",
                format!("{name}: reinstalled {to}, discarding local edits"),
                Vec::new(),
            ),
            Update::Moved { from, to, .. } => {
                ("updated", format!("{name}: {from} -> {to}"), Vec::new())
            }
            Update::Current(at) => ("current", format!("{name}: up to date at {at}"), Vec::new()),
            Update::Pinned(tag) => (
                "pinned",
                format!("{name}: pinned to {tag} by its ref; skipped"),
                Vec::new(),
            ),
            Update::Refused(reason, problems) => {
                refusals += 1;
                ("refused", format!("{name}: {reason}"), problems.clone())
            }
        };
        lines.push(line.clone());
        lines.extend(problems.iter().map(|problem| format!("  {problem}")));
        if let Some(note) = &held_back {
            lines.push(note.clone());
        }
        let requirements = match &result {
            Update::Moved { requirements, .. } => requirements.clone(),
            _ => RequirementReport::default(),
        };
        lines.extend(requirements.lines(name));
        results.push(json!({
            "name": name,
            "status": status,
            "summary": line,
            "problems": problems,
            "heldBack": held_back,
            "unmet": requirements.unmet,
            "mcpSnippets": requirements.snippets,
            "from": match &result { Update::Moved { from, .. } => Some(from.clone()), _ => None },
            "to": match &result { Update::Moved { to, .. } => Some(to.clone()), _ => None },
        }));
    }
    let body = json!({
        "ok": refusals == 0,
        "layer": layer_label(layer.kind),
        "packs": results,
    });
    if refusals > 0 {
        let mut body = body;
        body["error"] = json!(format!("{refusals} of {} packs not updated", names.len()));
        body["problems"] = json!(lines);
        return Ok(Outcome::rejected(body));
    }
    Ok(Outcome::ok(body).with_note(lines.join("\n  ")))
}

fn update_one(
    layer: &Layer,
    state: &layer::LayerState,
    entry: &PackEntry,
    name: &str,
    force: bool,
    lock: &mut graph_config::pack::PackLock,
    held_back: &mut Option<String>,
) -> Result<Update> {
    let copy = layer.copy_dir(name);
    let Some(locked) = lock.packs.get(name).cloned().filter(|_| copy.is_dir()) else {
        return Ok(Update::Refused(
            format!("not installed; run `{}`", layer.command("add", name)),
            Vec::new(),
        ));
    };
    let modified = content_hash(&copy)? != locked.hash;
    if modified && !force {
        return Ok(Update::Refused(
            format!(
                "{} has local edits (its content no longer matches {}); keep them by leaving the pack alone, or overwrite them with `{} --force`",
                copy.display(),
                layer.lock_path().display(),
                layer.command("update", name)
            ),
            Vec::new(),
        ));
    }
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
    let releases = refs::releases(&remote);
    if let (Some(line), Some(newest)) = (wanted.major_line(), refs::newest(&releases)) {
        if newest.version.major > line {
            *held_back = Some(format!(
                "{} available: {} --major",
                newest.tag,
                layer.command("update", name)
            ));
        }
    }
    let target = match refs::resolve(&remote, &wanted, &url) {
        Ok(target) => target,
        Err(problem) => return Ok(Update::Refused(problem, Vec::new())),
    };
    let from = locked.tag.clone().unwrap_or_else(|| short(&locked.sha));
    let to = target.tag.clone().unwrap_or_else(|| short(&target.sha));
    if target.sha == locked.sha && target.tag == locked.tag && !modified {
        return Ok(Update::Current(from));
    }
    let fetched = match layer::fetch(layer, &url, &target.reference) {
        Ok(fetched) => fetched,
        Err(problem) => return Ok(Update::Refused(problem, Vec::new())),
    };
    let (hash, loaded) = match fetched.verify(
        name,
        target.tag.as_deref(),
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
            git: locked.git.clone(),
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

pub fn remove(layer: &Layer, name: &str) -> Result<Outcome> {
    let state = layer.state()?;
    let copy = layer.copy_dir(name);
    let configured = state.packs.contains_key(name);
    let locked = state.lock.packs.contains_key(name);
    if !configured && !locked && !copy.exists() {
        return Ok(refusal(format!(
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
            return Err(error);
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
            return Err(error);
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
    Ok(Outcome::ok(json!({
        "ok": true,
        "name": name,
        "layer": layer_label(layer.kind),
        "removed": copy.display().to_string(),
        "callers": callers.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
    }))
    .with_note(lines.join("\n  ")))
}

pub fn list(layers: &[Layer], offline: bool) -> Result<Outcome> {
    let states = layers
        .iter()
        .map(|layer| layer.state().map(|state| (layer, state)))
        .collect::<Result<Vec<_>>>()?;
    let mut rows = Vec::new();
    let mut text = String::new();
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
                .map(|(later, _)| layer_label(later.kind));
            let mut row = json!({
                "name": name,
                "layer": layer_label(layer.kind),
                "dir": copy.display().to_string(),
                "git": entry.git,
                "ref": entry.reference,
                "installed": installed,
                "tag": locked.and_then(|locked| locked.tag.clone()),
                "sha": locked.map(|locked| locked.sha.clone()),
                "modified": modified,
                "shadowed": shadowed_by.is_some(),
                "shadowedBy": shadowed_by,
            });
            if !offline {
                remote_columns(&mut row, entry, locked);
            }
            text.push_str(&list_line(&row, offline));
            rows.push(row);
        }
    }
    let count = rows.len();
    let outcome = Outcome::raw(
        text,
        json!({"packs": rows, "count": count, "offline": offline}),
    );
    if count == 0 {
        return Ok(
            outcome.with_note("no packs installed; add one with `graph pack add <name|url>`")
        );
    }
    Ok(outcome)
}

fn remote_columns(row: &mut Value, entry: &PackEntry, locked: Option<&LockEntry>) {
    let url = remote_url(&entry.git);
    let remote = match git::ls_remote(&url) {
        Ok(remote) => remote,
        Err(problem) => {
            row["remoteError"] = json!(problem);
            return;
        }
    };
    let releases = refs::releases(&remote);
    row["newest"] = json!(refs::newest(&releases).map(|release| release.tag.clone()));
    let wanted = match locked {
        Some(locked) => wanted_ref(entry, locked),
        None => entry.reference.as_deref().map(PackRef::parse),
    };
    row["newestInRef"] = match wanted {
        Some(PackRef::Branch(branch)) => json!(remote
            .heads
            .get(&branch)
            .map(|sha| format!("{branch}@{}", short(sha)))),
        Some(wanted) => json!(wanted
            .major_line()
            .and_then(|major| refs::newest_in(&releases, major))
            .map(|release| release.tag.clone())),
        None => Value::Null,
    };
}

fn list_line(row: &Value, offline: bool) -> String {
    let text = |key: &str| row[key].as_str().unwrap_or("-").to_string();
    let installed = if row["installed"] == json!(true) {
        row["tag"]
            .as_str()
            .map(str::to_string)
            .or_else(|| row["sha"].as_str().map(short))
            .unwrap_or_else(|| "-".to_string())
    } else {
        "not installed".to_string()
    };
    let mut columns = vec![text("name"), text("layer"), installed];
    if !offline {
        match row.get("remoteError") {
            Some(_) => columns.push("remote unreachable".to_string()),
            None => {
                columns.push(format!("ref {}: {}", text("ref"), text("newestInRef")));
                columns.push(format!("newest: {}", text("newest")));
            }
        }
    }
    let mut flags = Vec::new();
    if row["modified"] == json!(true) {
        flags.push("local edits".to_string());
    }
    if let Some(by) = row["shadowedBy"].as_str() {
        flags.push(format!("shadowed by the {by} layer"));
    }
    if !flags.is_empty() {
        columns.push(flags.join(", "));
    }
    format!("{}\n", columns.join("\t"))
}

#[cfg(test)]
mod tests;
