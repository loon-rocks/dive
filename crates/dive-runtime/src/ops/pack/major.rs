use super::{
    installed_version, local_edits, not_configured, update_names, update_one, update_outcome,
    wanted_ref, CrossedColumns, CrossingRow, PackError, PackUpdates, Update,
};
use crate::packs::author::{self, PackSession, MIGRATION_PREFIX};
use crate::packs::git::{self, remote_url};
use crate::packs::layer::{self, Fetched, Layer, LayerState};
use crate::packs::refs::{self, PackRef, Release};
use crate::packs::RequirementReport;
use crate::runtime::RuntimeOptions;
use anyhow::Result;
use dive_config::pack::{LockEntry, PackEntry, PackLock};
use dive_config::Config;
use dive_core::packs::PackSource;
use dive_core::pipeline::{AskOutcome, AskRequest, Interlocutor, StepPath};
use dive_core::EventSink;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const GUIDE_FILE: &str = "MIGRATING.md";

pub struct Consent {
    pub yes: bool,
    pub asker: Option<Arc<dyn Interlocutor>>,
}

impl Consent {
    async fn confirm(&self, prompt: String) -> Result<(), Halt> {
        if self.yes {
            return Ok(());
        }
        let Some(asker) = &self.asker else {
            return Err(Halt::failed(
                "there is no terminal to ask on; rerun with --yes",
            ));
        };
        let outcome = asker
            .ask(AskRequest {
                path: StepPath::top("pack update --major"),
                call_stack: Vec::new(),
                prompt,
                schema: json!({
                    "type": "object",
                    "required": ["proceed"],
                    "properties": {
                        "proceed": {
                            "type": "boolean",
                            "description": "y continues, n stops here"
                        }
                    }
                }),
                default: None,
            })
            .await;
        match outcome {
            AskOutcome::Answered(answer) if answer["proceed"] == json!(true) => Ok(()),
            AskOutcome::Unavailable(reason) => {
                Err(Halt::failed(format!("could not ask for consent: {reason}")))
            }
            _ => Err(Halt {
                reason: "declined".to_string(),
                problems: Vec::new(),
                declined: true,
            }),
        }
    }
}

pub struct Majors<'a> {
    pub consent: &'a Consent,
    pub config: &'a Config,
    pub events: Arc<dyn EventSink>,
    pub options: RuntimeOptions,
}

#[derive(Debug, Clone)]
pub struct Halt {
    reason: String,
    problems: Vec<String>,
    declined: bool,
}

impl Halt {
    fn failed(reason: impl Into<String>) -> Self {
        Halt {
            reason: reason.into(),
            problems: Vec::new(),
            declined: false,
        }
    }

    fn with(reason: impl Into<String>, problems: Vec<String>) -> Self {
        Halt {
            problems,
            ..Halt::failed(reason)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Crossing {
    from: String,
    to: String,
    migration: Option<String>,
    files: Vec<Value>,
    rewrote: Vec<String>,
    guide: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Crossed {
    pub from: String,
    crossings: Vec<Crossing>,
    halt: Option<Halt>,
    left: Vec<String>,
    pub requirements: RequirementReport,
}

impl Crossed {
    pub fn to(&self) -> Option<String> {
        self.crossings.last().map(|crossing| crossing.to.clone())
    }

    fn at(&self) -> String {
        self.to().unwrap_or_else(|| self.from.clone())
    }

    pub fn status(&self) -> &'static str {
        match &self.halt {
            None => "updated",
            Some(halt) if halt.declined => "declined",
            Some(_) => "refused",
        }
    }

    pub fn summary(&self, name: &str, layer: &Layer) -> String {
        let Some(halt) = &self.halt else {
            return format!("{name}: {} -> {}", self.from, self.at());
        };
        format!(
            "{name}: stopped at {} ({}); not crossed: {}; continue with `{} --major`",
            self.at(),
            match halt.declined {
                true => format!("{} declined", self.left[0]),
                false => format!("{}: {}", self.left[0], halt.reason),
            },
            self.left.join(", "),
            layer.command("update", name)
        )
    }

    pub fn problems(&self) -> Vec<String> {
        self.halt
            .as_ref()
            .map(|halt| halt.problems.clone())
            .unwrap_or_default()
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for crossing in &self.crossings {
            let how = match &crossing.migration {
                Some(plan) => match crossing.rewrote.len() {
                    0 => format!("{plan} changed no files"),
                    count => format!(
                        "{plan} rewrote {count} file(s): {}",
                        crossing.rewrote.join(", ")
                    ),
                },
                None => "no migration plan; update your files by hand".to_string(),
            };
            lines.push(format!("  {} -> {}: {how}", crossing.from, crossing.to));
            if crossing.migration.is_none() {
                match &crossing.guide {
                    Some(guide) => lines.extend(guide.lines().map(|line| format!("    {line}"))),
                    None => lines.push(format!("    {GUIDE_FILE} has no section for this major")),
                }
            }
        }
        lines
    }

    pub fn columns(&self) -> CrossedColumns {
        CrossedColumns {
            crossed: self
                .crossings
                .iter()
                .map(|crossing| CrossingRow {
                    from: crossing.from.clone(),
                    to: crossing.to.clone(),
                    migration: crossing.migration.clone(),
                    files: crossing.files.clone(),
                    guide: crossing.guide.clone(),
                })
                .collect(),
            stopped_at: self.halt.as_ref().map(|_| self.at()),
            left: self.left.clone(),
        }
    }
}

const MIGRATIONS_DIR: &str = "plans/migrations";

struct Run<'a> {
    layer: &'a Layer,
    root: PathBuf,
    state: &'a LayerState,
    force: bool,
    majors: &'a Majors<'a>,
}

pub async fn update(
    layer: &Layer,
    names: &[String],
    force: bool,
    majors: &Majors<'_>,
) -> Result<PackUpdates, PackError> {
    if !majors.consent.yes && majors.consent.asker.is_none() {
        return Err(PackError::NoTerminal(
            "`graph pack update --major` shows each migration's changes and asks before applying them, and there is no terminal to ask on; nothing was changed. Rerun with --yes to accept every major without asking".to_string(),
        ));
    }
    let state = layer.state()?;
    let names = match update_names(layer, &state, names) {
        Ok(names) => names,
        Err(empty) => return Ok(*empty),
    };
    let run = Run {
        layer,
        root: std::path::absolute(&layer.dir).unwrap_or_else(|_| layer.dir.clone()),
        state: &state,
        force,
        majors,
    };
    let mut lock = state.lock.clone();
    let mut results = Vec::new();
    for name in names {
        let mut held_back = None;
        let result = match state.packs.get(&name) {
            None => not_configured(layer, &name),
            Some(entry) => cross_majors(&run, entry, &name, &mut lock, &mut held_back).await?,
        };
        results.push((name, result, held_back));
    }
    Ok(update_outcome(layer, results))
}

async fn cross_majors(
    run: &Run<'_>,
    entry: &PackEntry,
    name: &str,
    lock: &mut PackLock,
    held_back: &mut Option<String>,
) -> Result<Update> {
    let copy = run.layer.copy_dir(name);
    let locked = lock.packs.get(name).filter(|_| copy.is_dir()).cloned();
    let line = match locked.as_ref().and_then(|locked| wanted_ref(entry, locked)) {
        Some(PackRef::Major(line)) => line,
        Some(PackRef::Branch(branch)) => return Ok(Update::Branch(branch)),
        _ => {
            return update_one(
                run.layer, run.state, entry, name, run.force, lock, held_back,
            )
        }
    };
    let locked = locked.expect("a major line comes from a lock entry");
    if let Err(refusal) = local_edits(run.layer, name, &copy, &locked, run.force)? {
        return Ok(refusal);
    }
    let url = remote_url(&entry.git);
    let remote = match git::ls_remote(&url) {
        Ok(remote) => remote,
        Err(problem) => return Ok(Update::Refused(problem, Vec::new())),
    };
    if let Some(moved) = refs::moved_tag(&remote, &entry.git, &entry.path, &locked) {
        return Ok(Update::Refused(moved, Vec::new()));
    }
    let releases = refs::releases(&remote, &entry.path);
    let newer: BTreeSet<u64> = releases
        .iter()
        .map(|release| release.version.major)
        .filter(|major| *major > line)
        .collect();
    let targets: Vec<&Release> = newer
        .into_iter()
        .filter_map(|major| refs::newest_in(&releases, major))
        .collect();
    if targets.is_empty() {
        return update_one(
            run.layer, run.state, entry, name, run.force, lock, held_back,
        );
    }
    let mut crossed = Crossed {
        from: installed_version(&locked),
        crossings: Vec::new(),
        halt: None,
        left: Vec::new(),
        requirements: RequirementReport::default(),
    };
    for (index, release) in targets.iter().enumerate() {
        match cross(run, entry, name, lock, release, &crossed.at()).await {
            Ok((crossing, requirements)) => {
                crossed.crossings.push(crossing);
                crossed.requirements = requirements;
            }
            Err(halt) => {
                crossed.halt = Some(halt);
                crossed.left = targets[index..]
                    .iter()
                    .map(|release| release.label())
                    .collect();
                break;
            }
        }
    }
    Ok(Update::Crossed(crossed))
}

async fn cross(
    run: &Run<'_>,
    entry: &PackEntry,
    name: &str,
    lock: &mut PackLock,
    release: &Release,
    from: &str,
) -> Result<(Crossing, RequirementReport), Halt> {
    let major = release.version.major;
    let to = release.label();
    let fetched = layer::fetch(
        run.layer,
        &remote_url(&entry.git),
        &release.target(),
        &entry.path,
    )
    .map_err(Halt::failed)?;
    let (hash, loaded) = fetched
        .verify(
            name,
            Some(&to),
            run.layer,
            Some(entry),
            &run.state.mcp_servers,
        )
        .map_err(|problems| Halt::with(format!("{to} does not load"), problems))?;
    let with_migrations = author::load_with_migrations(PackSource {
        name: name.to_string(),
        tag: Some(to.clone()),
        layer: run.layer.kind,
        dir: fetched.path.clone(),
    })
    .map_err(|problems| Halt::with(format!("{to}'s migrations do not load"), problems))?;
    let plan = format!("{MIGRATION_PREFIX}{major}");
    let mut crossing = Crossing {
        from: from.to_string(),
        to: to.clone(),
        migration: None,
        files: Vec::new(),
        rewrote: Vec::new(),
        guide: None,
    };
    let title = format!("{name} {from} -> {to}");
    let backup = if author::migrations(&with_migrations).contains(&major) {
        let session = PackSession::open(
            run.majors.config,
            &run.majors.options,
            with_migrations,
            run.majors.events.clone(),
        )
        .map_err(|error| Halt::failed(format!("starting {plan}: {error:#}")))?;
        let migrated = migrate(run, &session, major, &title).await;
        session.shutdown().await;
        let (files, backup) = migrated?;
        crossing.migration = Some(plan);
        crossing.rewrote = changed_paths(&files)
            .iter()
            .map(|path| shown(run, path))
            .collect();
        crossing.files = files;
        Some(backup)
    } else {
        crossing.guide = guide(&fetched.path, major);
        run.majors
            .consent
            .confirm(guide_prompt(
                &title,
                &plan,
                major,
                crossing.guide.as_deref(),
            ))
            .await?;
        None
    };
    let locked = LockEntry {
        git: entry.git.clone(),
        path: entry.path.clone(),
        tag: Some(release.tag.clone()),
        sha: fetched.sha.clone(),
        hash,
    };
    let installed = install(run.layer, name, lock, &fetched, locked, major);
    if let Err(error) = installed {
        if let Some(backup) = backup {
            backup.restore();
        }
        return Err(Halt::failed(format!(
            "installing {to}: {error:#}; your files are restored"
        )));
    }
    Ok((
        crossing,
        RequirementReport::of(&loaded, &run.state.mcp_servers),
    ))
}

async fn migrate(
    run: &Run<'_>,
    session: &PackSession,
    major: u64,
    title: &str,
) -> Result<(Vec<Value>, Backup), Halt> {
    let plan = format!("{MIGRATION_PREFIX}{major}");
    let root = &run.root;
    let dry = session
        .migrate(major, root, false)
        .await
        .map_err(|error| Halt::failed(format!("{plan} failed its dry run: {error}")))?;
    run.majors
        .consent
        .confirm(format!(
            "{title}: {plan} proposes these changes to your files in {}:\n\n{}\n\nApply them and install the new major?",
            run.layer.dir.display(),
            render(&dry)
        ))
        .await?;
    let backup = Backup::take(root, &dry);
    match session.migrate(major, root, true).await {
        Ok(applied) => Ok((files(&applied), backup)),
        Err(error) => {
            backup.restore();
            Err(Halt::failed(format!(
                "{plan} failed: {error}; the files its dry run named are restored"
            )))
        }
    }
}

fn install(
    layer: &Layer,
    name: &str,
    lock: &mut PackLock,
    fetched: &Fetched,
    locked: LockEntry,
    major: u64,
) -> Result<()> {
    let swap = fetched.install(&layer.copy_dir(name))?;
    let config_before = layer.read_config()?;
    let written = layer::with_pack_ref(&config_before, name, &format!("v{major}"))
        .and_then(|text| layer.write_config(&text));
    if let Err(error) = written {
        swap.undo();
        return Err(error);
    }
    let previous = lock.packs.insert(name.to_string(), locked);
    if let Err(error) = layer.write_lock(lock) {
        if let Some(previous) = previous {
            lock.packs.insert(name.to_string(), previous);
        }
        let _ = layer.write_config(&config_before);
        swap.undo();
        return Err(error);
    }
    Ok(())
}

fn files(output: &Value) -> Vec<Value> {
    output
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn changed(output: &Value) -> Vec<Value> {
    files(output)
        .into_iter()
        .filter(|file| file.get("changed") != Some(&json!(false)))
        .collect()
}

fn shown(run: &Run<'_>, path: &str) -> String {
    let path = Path::new(path);
    let inside = path.strip_prefix(&run.root).unwrap_or(path);
    match inside.is_relative() {
        true => run.layer.dir.join(inside).display().to_string(),
        false => inside.display().to_string(),
    }
}

fn changed_paths(files: &[Value]) -> Vec<String> {
    files
        .iter()
        .filter(|file| file.get("changed") != Some(&json!(false)))
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn render(output: &Value) -> String {
    if !output.get("files").is_some_and(Value::is_array) {
        return serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string());
    }
    let changed = changed(output);
    if changed.is_empty() {
        return "(no files to change)".to_string();
    }
    changed
        .iter()
        .map(|file| {
            let path = file.get("path").and_then(Value::as_str).unwrap_or("(file)");
            match file.get("diff").and_then(Value::as_str) {
                Some(diff) if !diff.trim().is_empty() => diff.trim_end().to_string(),
                _ => format!("{path}: changed (the migration gave no diff)"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn guide(dir: &Path, major: u64) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(GUIDE_FILE)).ok()?;
    let heading = format!("## v{} to v{major}", major.saturating_sub(1));
    let mut lines = text.lines().skip_while(|line| line.trim_end() != heading);
    lines.next()?;
    let section: Vec<&str> = lines
        .take_while(|line| !line.starts_with("# ") && !line.starts_with("## "))
        .collect();
    Some(section.join("\n").trim().to_string())
}

fn guide_prompt(title: &str, plan: &str, major: u64, guide: Option<&str>) -> String {
    let heading = format!("## v{} to v{major}", major.saturating_sub(1));
    match guide {
        Some(guide) => format!(
            "{title}: the new major has no {MIGRATIONS_DIR}/{plan}.yaml, so your files are not migrated. Its {GUIDE_FILE}, {heading}:\n\n{guide}\n\nInstall the new major and update your files by hand?"
        ),
        None => format!(
            "{title}: the new major has no {MIGRATIONS_DIR}/{plan}.yaml, and its {GUIDE_FILE} has no {heading} section, so there is no guide to what changed.\n\nInstall the new major anyway?"
        ),
    }
}

struct Backup(Vec<(PathBuf, Option<Vec<u8>>)>);

impl Backup {
    fn take(root: &Path, dry: &Value) -> Backup {
        Backup(
            changed_paths(&changed(dry))
                .into_iter()
                .map(|path| {
                    let path = root.join(path);
                    let content = std::fs::read(&path).ok();
                    (path, content)
                })
                .collect(),
        )
    }

    fn restore(self) {
        for (path, content) in self.0 {
            let _ = match content {
                Some(content) => std::fs::write(&path, content),
                None => std::fs::remove_file(&path),
            };
        }
    }
}
