use super::{refusal, refused};
use crate::commands::outcome::Outcome;
use anyhow::Result;
use dive_config::pack::{check_pack_name, MANIFEST_FILE};
use dive_config::Config;
use dive_core::packs::{LoadedPack, PackSource};
use dive_core::EventSink;
use dive_runtime::packs::author::{self, PackSession, Report};
use dive_runtime::packs::release::{self, Bump};
use dive_runtime::packs::{git, refs, scaffold};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

fn repo_path(top: &Path, dir: &Path) -> Result<String, String> {
    let canonical = |path: &Path| {
        path.canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))
    };
    let (top, dir) = (canonical(top)?, canonical(dir)?);
    let inside = dir.strip_prefix(&top).map_err(|_| {
        format!(
            "{} is not inside the git repository at {}",
            dir.display(),
            top.display()
        )
    })?;
    let parts: Vec<String> = inside
        .components()
        .filter_map(|part| match part {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    Ok(parts.join("/"))
}

fn config() -> Result<Config> {
    Ok(crate::host::load_config()?.config)
}

fn events(json: bool, name: &str) -> Arc<dyn EventSink> {
    crate::output::make_sink(json, false, crate::telemetry::RunInfo::plan_run(name))
}

pub fn new_pack(name: &str, root: &Path) -> Result<Outcome> {
    if let Err(problem) = check_pack_name(name) {
        return Ok(refusal(problem));
    }
    if root.exists() && !root.is_dir() {
        return Ok(refusal(format!("{} is not a directory", root.display())));
    }
    let dir = root.join(name);
    if dir.exists() {
        if !dir.is_dir() {
            return Ok(refusal(format!("{} is not a directory", dir.display())));
        }
        if std::fs::read_dir(&dir)?.next().is_some() {
            return Ok(refusal(format!(
                "{} is not empty; scaffold into an empty or new directory",
                dir.display()
            )));
        }
    }
    let mut written = scaffold::write(&dir, name)?;
    written.extend(scaffold::write_repo(root)?);
    Ok(Outcome::ok(json!({
        "ok": true,
        "name": name,
        "savedTo": dir.display().to_string(),
        "files": written
            .iter()
            .map(|file| file.display().to_string())
            .collect::<Vec<_>>(),
    }))
    .with_note(format!(
        "scaffolded pack '{name}'; run `dive pack check {name}` and `dive pack test {name}` in {}, and release it with `dive pack release <bump> {name}` as {name}/vX.Y.Z tags",
        root.display()
    )))
}

enum Scope {
    Pack,
    Repo(Vec<PathBuf>),
}

fn scope(path: &Path) -> Result<Scope, Outcome> {
    if path.join(MANIFEST_FILE).is_file() {
        return Ok(Scope::Pack);
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|dir| dir.join(MANIFEST_FILE).is_file())
                .collect()
        })
        .unwrap_or_default();
    if dirs.is_empty() {
        return Err(refusal(format!(
            "{} has no {MANIFEST_FILE}, and none of its subdirectories has one; is it a pack or a packs repository?",
            path.display()
        )));
    }
    dirs.sort();
    Ok(Scope::Repo(dirs))
}

fn combined(verb: &str, path: &Path, outcomes: Vec<(PathBuf, Outcome)>) -> Outcome {
    let count = outcomes.len();
    let mut packs = Vec::new();
    let mut problems = Vec::new();
    let mut lines = Vec::new();
    let mut failed = 0;
    for (dir, outcome) in outcomes {
        let shown = dir.display().to_string();
        if outcome.rejected {
            failed += 1;
            let listed: Vec<String> = outcome.body["problems"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            if listed.is_empty() {
                problems.push(format!(
                    "{shown}: {}",
                    outcome.body["error"].as_str().unwrap_or("failed")
                ));
            }
            problems.extend(listed.iter().map(|problem| format!("{shown}: {problem}")));
        } else if let Some(note) = outcome.body["note"].as_str() {
            lines.push(format!("{shown}: {}", note.replace('\n', "\n  ")));
        }
        packs.push(outcome.body);
    }
    let body = json!({
        "ok": failed == 0,
        "dir": path.display().to_string(),
        "packs": packs,
        "problems": problems,
    });
    if failed > 0 {
        let mut body = body;
        body["error"] = json!(format!(
            "{failed} of {count} packs in {} fail {verb}",
            path.display()
        ));
        return Outcome::rejected(body);
    }
    Outcome::ok(body).with_note(lines.join("\n  "))
}

fn items(loaded: &LoadedPack) -> Vec<String> {
    loaded
        .items
        .iter()
        .map(|item| item.catalog_name())
        .filter(|name| !loaded.is_migration(name))
        .collect()
}

fn check_body(loaded: &LoadedPack, report: &Report) -> Value {
    json!({
        "ok": report.problems.is_empty(),
        "name": loaded.source.name,
        "dir": loaded.source.dir.display().to_string(),
        "items": items(loaded),
        "migrations": loaded.migrations,
        "problems": report.problems,
        "notes": report.notes,
    })
}

fn problem_count(count: usize) -> String {
    match count {
        1 => "1 problem".to_string(),
        count => format!("{count} problems"),
    }
}

pub async fn check(path: &Path) -> Result<Outcome> {
    let dirs = match scope(path) {
        Ok(Scope::Pack) => return check_pack(&config()?, path).await,
        Ok(Scope::Repo(dirs)) => dirs,
        Err(outcome) => return Ok(outcome),
    };
    let config = config()?;
    let mut outcomes = Vec::new();
    for dir in dirs {
        let outcome = check_pack(&config, &dir).await?;
        outcomes.push((dir, outcome));
    }
    Ok(combined("check", path, outcomes))
}

async fn check_pack(config: &Config, path: &Path) -> Result<Outcome> {
    let (loaded, report) = author::check(config, &crate::host::options(), path).await?;
    let mut body = check_body(&loaded, &report);
    if !report.problems.is_empty() {
        body["error"] = json!(format!(
            "pack '{}' at {} has {}",
            loaded.source.name,
            path.display(),
            problem_count(report.problems.len())
        ));
        return Ok(Outcome::rejected(body));
    }
    let mut lines = vec![format!(
        "pack '{}' loads: {} items, {} migration items",
        loaded.source.name,
        items(&loaded).len(),
        loaded.migrations.len()
    )];
    lines.extend(report.notes.iter().cloned());
    Ok(Outcome::ok(body).with_note(lines.join("\n  ")))
}

fn loaded_for_run(path: &Path) -> Result<LoadedPack, Outcome> {
    if !path.join(dive_config::pack::MANIFEST_FILE).is_file() {
        return Err(refusal(format!(
            "{} has no {}; is it a pack?",
            path.display(),
            dive_config::pack::MANIFEST_FILE
        )));
    }
    author::load_with_migrations(author::source_at(path)).map_err(|problems| {
        refused(
            format!(
                "the pack at {} does not load; `dive pack check` lists every problem",
                path.display()
            ),
            problems,
        )
    })
}

pub async fn run_item(path: &Path, item: &str, input: Value, json: bool) -> Result<Outcome> {
    let loaded = match loaded_for_run(path) {
        Ok(loaded) => loaded,
        Err(outcome) => return Ok(outcome),
    };
    let session = PackSession::open(
        &config()?,
        &crate::host::options(),
        loaded,
        events(json, item),
    )?;
    let resolved = session.resolve(item);
    let result = match &resolved {
        Ok(name) => session.run(name, input).await,
        Err(problem) => Err(problem.clone()),
    };
    session.shutdown().await;
    let usage = session.usage();
    let result = match result {
        Ok(result) => result,
        Err(problem) => return Ok(refusal(problem)),
    };
    let text = format!("{}\n", serde_json::to_string_pretty(&result)?);
    let outcome = Outcome::raw(
        text,
        json!({"ok": true, "item": resolved.ok(), "result": result, "usage": usage}),
    );
    Ok(match usage {
        Some(usage) => outcome.with_note(usage),
        None => outcome,
    })
}

fn ran(case: &author::CaseRun) -> Vec<String> {
    case.ran
        .iter()
        .map(|major| format!("{}{major}", author::MIGRATION_PREFIX))
        .collect()
}

struct Tested {
    loaded: LoadedPack,
    check: Report,
    cases: Vec<author::CaseRun>,
}

impl Tested {
    fn problems(&self) -> Vec<String> {
        self.check
            .problems
            .iter()
            .cloned()
            .chain(self.cases.iter().flat_map(|case| case.problems.clone()))
            .collect()
    }

    fn cases_json(&self) -> Value {
        json!(self
            .cases
            .iter()
            .map(|case| json!({
                "case": case.case,
                "ran": ran(case),
                "problems": case.problems,
            }))
            .collect::<Vec<_>>())
    }

    fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "pack '{}' passes check: {} items, {} migration items",
            self.loaded.source.name,
            items(&self.loaded).len(),
            self.loaded.migrations.len()
        )];
        lines.extend(self.check.notes.iter().cloned());
        if self.cases.is_empty() {
            lines.push(format!(
                "no {}/v<N>/ sample files to migrate",
                author::MIGRATION_TESTS
            ));
        }
        for case in &self.cases {
            lines.push(format!(
                "{}: ran {}; the migrated files validate",
                case.case,
                ran(case).join(", ")
            ));
        }
        lines
    }
}

async fn run_tests(config: &Config, path: &Path, json: bool) -> Result<Tested> {
    let (loaded, check) = author::check(config, &crate::host::options(), path).await?;
    let mut tested = Tested {
        loaded,
        check,
        cases: Vec::new(),
    };
    if !tested.check.problems.is_empty() {
        return Ok(tested);
    }
    let cases = author::migration_cases(path);
    if cases.is_empty() {
        return Ok(tested);
    }
    let majors = author::migrations(&tested.loaded);
    let session = PackSession::open(
        config,
        &crate::host::options(),
        tested.loaded.clone(),
        events(json, &format!("{} migrations", tested.loaded.source.name)),
    )?;
    for (major, case) in cases {
        match author::run_migration_case(&session, config, &majors, major, &case).await {
            Ok(run) => tested.cases.push(run),
            Err(error) => {
                session.shutdown().await;
                return Err(error);
            }
        }
    }
    session.shutdown().await;
    Ok(tested)
}

pub async fn test(path: &Path, json: bool) -> Result<Outcome> {
    let dirs = match scope(path) {
        Ok(Scope::Pack) => return test_pack(&config()?, path, json).await,
        Ok(Scope::Repo(dirs)) => dirs,
        Err(outcome) => return Ok(outcome),
    };
    let config = config()?;
    let mut outcomes = Vec::new();
    for dir in dirs {
        let outcome = test_pack(&config, &dir, json).await?;
        outcomes.push((dir, outcome));
    }
    Ok(combined("their tests", path, outcomes))
}

async fn test_pack(config: &Config, path: &Path, json: bool) -> Result<Outcome> {
    let tested = run_tests(config, path, json).await?;
    let problems = tested.problems();
    let mut body = check_body(&tested.loaded, &tested.check);
    body["cases"] = tested.cases_json();
    body["problems"] = json!(problems);
    body["ok"] = json!(problems.is_empty());
    if !problems.is_empty() {
        body["error"] = json!(format!(
            "pack '{}' at {} fails its tests with {}",
            tested.loaded.source.name,
            path.display(),
            problem_count(problems.len())
        ));
        return Ok(Outcome::rejected(body));
    }
    Ok(Outcome::ok(body).with_note(tested.lines().join("\n  ")))
}

fn previous_interface(
    top: &Path,
    tag: &str,
    prefix: &str,
    name: &str,
) -> Result<std::collections::BTreeMap<String, release::Signature>> {
    let scratch = tempfile::tempdir()?;
    let tree = scratch.path().join("tree");
    git::export(top, tag, prefix, &tree).map_err(anyhow::Error::msg)?;
    let (loaded, _) = author::inspect(PackSource {
        name: name.to_string(),
        tag: Some(tag.to_string()),
        layer: dive_config::pack::LayerKind::Project,
        dir: tree,
    });
    Ok(release::interface(&loaded))
}

pub async fn release(path: &Path, bump: Bump, json: bool) -> Result<Outcome> {
    let top = match git::toplevel(path) {
        Ok(top) => top,
        Err(problem) => {
            return Ok(refused(
                format!("{} is not in a git repository", path.display()),
                vec![problem],
            ))
        }
    };
    if !path.join(MANIFEST_FILE).is_file() {
        return Ok(refusal(format!(
            "{} has no {MANIFEST_FILE}; release runs on one pack, so name its directory, as in `dive pack release <bump> <dir>`",
            path.display()
        )));
    }
    let prefix = match repo_path(&top, path) {
        Ok(prefix) => prefix,
        Err(problem) => return Ok(refusal(problem)),
    };
    let dirty = git::uncommitted(path).map_err(anyhow::Error::msg)?;
    if !dirty.is_empty() {
        return Ok(refused(
            format!(
                "{} has uncommitted changes; commit or stash them before releasing",
                path.display()
            ),
            dirty,
        ));
    }
    let tags = git::local_tags(path).map_err(anyhow::Error::msg)?;
    let previous = release::previous_release(&tags, &prefix);
    let (current, _) = author::inspect(author::source_at(path));
    let name = current.source.name.clone();
    let changes = match &previous {
        Some((tag, _)) => release::compare(
            &previous_interface(&top, tag, &prefix, &name)?,
            &release::interface(&current),
        ),
        None => Vec::new(),
    };
    let needed = release::required(&changes);
    let change_lines: Vec<String> = changes
        .iter()
        .map(|change| format!("{} ({})", change.what, change.bump.as_str()))
        .collect();
    if bump < needed {
        return Ok(refused(
            format!(
                "a {} release can't carry these changes since {}; they need a {}",
                bump.as_str(),
                previous
                    .as_ref()
                    .map(|(tag, _)| tag.as_str())
                    .unwrap_or("-"),
                needed.as_str()
            ),
            change_lines,
        ));
    }
    let next = bump.next(previous.as_ref().map(|(_, version)| version));
    if bump == Bump::Major
        && previous.is_some()
        && !author::migrations(&current).contains(&next.major)
        && !release::migrating_section(path, next.major)
    {
        return Ok(refusal(format!(
            "a major release to v{} needs plans/migrations/{}{}.yaml or a `## v{} to v{}` section in {}",
            next.major,
            author::MIGRATION_PREFIX,
            next.major,
            next.major - 1,
            next.major,
            release::MIGRATING_FILE
        )));
    }

    let tested = run_tests(&config()?, path, json).await?;
    let problems = tested.problems();
    if !problems.is_empty() {
        return Ok(refused(
            format!(
                "`dive pack test` fails with {}; nothing was tagged",
                problem_count(problems.len())
            ),
            problems,
        ));
    }
    let tag = refs::release_tag(&prefix, &next);
    let sha =
        git::annotated_tag(path, &tag, &format!("{name} {tag}")).map_err(anyhow::Error::msg)?;
    let mut lines = vec![format!(
        "tagged {tag} at {} ({} release{})",
        &sha[..sha.len().min(7)],
        bump.as_str(),
        previous
            .as_ref()
            .map(|(tag, _)| format!(" after {tag}"))
            .unwrap_or_else(|| ", the first".to_string())
    )];
    lines.extend(change_lines.iter().cloned());
    lines.push(format!(
        "nothing was pushed; publish it with `git push origin {tag}`"
    ));
    Ok(Outcome::ok(json!({
        "ok": true,
        "name": name,
        "path": prefix,
        "tag": tag,
        "version": format!("v{next}"),
        "sha": sha,
        "bump": bump.as_str(),
        "previous": previous.as_ref().map(|(tag, _)| tag.clone()),
        "changes": changes
            .iter()
            .map(|change| json!({"bump": change.bump.as_str(), "change": change.what}))
            .collect::<Vec<_>>(),
        "pushed": false,
    }))
    .with_note(lines.join("\n  ")))
}
