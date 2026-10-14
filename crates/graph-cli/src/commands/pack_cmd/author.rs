use super::{refusal, refused};
use crate::commands::outcome::Outcome;
use crate::packs::author::{self, PackSession, Report};
use crate::packs::release::{self, Bump};
use crate::packs::{git, scaffold};
use anyhow::Result;
use graph_config::pack::check_pack_name;
use graph_config::Config;
use graph_core::packs::{LoadedPack, PackSource};
use graph_core::EventSink;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

fn config() -> Result<Config> {
    Ok(crate::runtime::load_config()?.config)
}

fn events(json: bool, name: &str) -> Arc<dyn EventSink> {
    crate::output::make_sink(json, false, crate::telemetry::RunInfo::plan_run(name))
}

pub fn new_pack(name: &str, path: &Path) -> Result<Outcome> {
    if let Err(problem) = check_pack_name(name) {
        return Ok(refusal(problem));
    }
    if path.exists() {
        if !path.is_dir() {
            return Ok(refusal(format!("{} is not a directory", path.display())));
        }
        if std::fs::read_dir(path)?.next().is_some() {
            return Ok(refusal(format!(
                "{} is not empty; scaffold into an empty or new directory",
                path.display()
            )));
        }
    }
    let written = scaffold::write(path, name)?;
    Ok(Outcome::ok(json!({
        "ok": true,
        "name": name,
        "savedTo": path.display().to_string(),
        "files": written
            .iter()
            .map(|file| file.display().to_string())
            .collect::<Vec<_>>(),
    }))
    .with_note(format!(
        "scaffolded pack '{name}'; run `graph pack check` and `graph pack test` in {}",
        path.display()
    )))
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
    let (loaded, report) = author::check(&config()?, path).await?;
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
    if !path.join(graph_config::pack::MANIFEST_FILE).is_file() {
        return Err(refusal(format!(
            "{} has no {}; is it a pack?",
            path.display(),
            graph_config::pack::MANIFEST_FILE
        )));
    }
    author::load_with_migrations(author::source_at(path)).map_err(|problems| {
        refused(
            format!(
                "the pack at {} does not load; `graph pack check` lists every problem",
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
    let session = PackSession::open(&config()?, loaded, events(json, item))?;
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
                "ran": case.ran.iter().map(|major| format!("{}{major}", author::MIGRATION_PREFIX)).collect::<Vec<_>>(),
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
            let ran: Vec<String> = case
                .ran
                .iter()
                .map(|major| format!("{}{major}", author::MIGRATION_PREFIX))
                .collect();
            lines.push(format!(
                "{}: ran {}; the migrated files validate",
                case.case,
                ran.join(", ")
            ));
        }
        lines
    }
}

async fn run_tests(config: &Config, path: &Path, json: bool) -> Result<Tested> {
    let (loaded, check) = author::check(config, path).await?;
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
    let tested = run_tests(&config()?, path, json).await?;
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
    path: &Path,
    tag: &str,
    name: &str,
) -> Result<std::collections::BTreeMap<String, release::Signature>> {
    let scratch = tempfile::tempdir()?;
    let tree = scratch.path().join("tree");
    git::export(path, tag, &tree).map_err(anyhow::Error::msg)?;
    let (loaded, _) = author::inspect(PackSource {
        name: name.to_string(),
        tag: Some(tag.to_string()),
        layer: graph_config::pack::LayerKind::Project,
        dir: tree,
    });
    Ok(release::interface(&loaded))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
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
    if !same_dir(&top, path) {
        return Ok(refusal(format!(
            "release runs at the root of the pack's repository, {}; {} is inside it",
            top.display(),
            path.display()
        )));
    }
    let dirty = git::uncommitted(path).map_err(anyhow::Error::msg)?;
    if !dirty.is_empty() {
        return Ok(refused(
            "the git tree has uncommitted changes; commit or stash them before releasing",
            dirty,
        ));
    }
    let tags = git::local_tags(path).map_err(anyhow::Error::msg)?;
    let previous = release::previous_release(&tags);
    let (current, _) = author::inspect(author::source_at(path));
    let name = current.source.name.clone();
    let changes = match &previous {
        Some((tag, _)) => release::compare(
            &previous_interface(path, tag, &name)?,
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
                "`graph pack test` fails with {}; nothing was tagged",
                problem_count(problems.len())
            ),
            problems,
        ));
    }
    let tag = format!("v{next}");
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
        "tag": tag,
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
