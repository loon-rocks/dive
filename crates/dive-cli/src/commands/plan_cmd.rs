//! `dive plan` — list/show/validate/run plan documents.

use crate::cli::PlanCommand;
use crate::commands::input::resolve_input;
use crate::commands::outcome::{report, Outcome};
use crate::commands::plan_edit;
use crate::output::SilentExit;
use anyhow::{bail, Result};
use dive_core::pipeline::catalog;
use dive_core::pipeline::doc::validate_input;
use dive_runtime::ops;
use std::sync::Arc;

/// Exit code for "the plan needs inputs you didn't provide".
const EXIT_NEEDS_INPUT: i32 = 3;
/// Exit code for "an exit step asserted failure" — distinct from 1
/// (infrastructure failure) so CI can branch on it.
const EXIT_PLAN_ASSERTED: i32 = 4;

pub async fn run(command: PlanCommand) -> Result<()> {
    match command {
        PlanCommand::List { json } => report(list()?, json),
        PlanCommand::Show { name, json } => report(show(&name)?, json),
        PlanCommand::Validate { name_or_path, json } => {
            let outcome = validate(&name_or_path)?;
            if json {
                return report(outcome, true);
            }
            render_validation(&outcome.body)
        }
        PlanCommand::Migrate { name_or_path, json } => report(migrate(&name_or_path)?, json),
        PlanCommand::Rm { name, json } => report(
            crate::commands::remove_cmd::run(dive_runtime::ops::remove::ItemKind::Plan, &name)?,
            json,
        ),
        PlanCommand::Run {
            name,
            input,
            inputs,
            json,
        } => run_plan(&name, input.as_deref(), &inputs, json).await,
        PlanCommand::New {
            identifier,
            name,
            description,
            output,
            json,
        } => report(
            plan_edit::new_plan(
                &crate::host::runtime()?,
                &identifier,
                name.as_deref(),
                description.as_deref(),
                output,
            )?,
            json,
        ),
        PlanCommand::Draft {
            goal,
            from,
            output,
            stdout,
            json,
        } => {
            let outcome = plan_edit::draft(
                &crate::host::runtime()?,
                &goal,
                from.as_deref(),
                output,
                stdout,
            )
            .await?;
            // `--stdout` asks for the document itself, so it outranks
            // `--json`: a caller who wanted raw YAML did not want it wrapped.
            report(outcome, json && !stdout)
        }
        PlanCommand::Set {
            target,
            attribute,
            value,
            json,
        } => report(
            plan_edit::set(&crate::host::runtime()?, &target, attribute, &value)?,
            json,
        ),
        PlanCommand::Unset {
            target,
            attribute,
            json,
        } => report(
            plan_edit::unset(&crate::host::runtime()?, &target, attribute)?,
            json,
        ),
        PlanCommand::Step { command } => {
            let json = command.json();
            report(plan_edit::step(&crate::host::runtime()?, command)?, json)
        }
    }
}

fn list() -> Result<Outcome> {
    let runtime = crate::host::runtime()?;
    let listed = ops::plan::list(&runtime);
    let body = serde_json::to_value(&listed)?;
    if listed.is_empty() {
        return Ok(Outcome::raw(String::new(), body)
            .with_note("no plan documents found — add YAML files under [plans].paths"));
    }
    let text = listed
        .plans
        .iter()
        .map(|plan| match &plan.pack {
            Some(source) => format!("{}\t{}\t{}\n", plan.identifier, plan.name, source),
            None => format!("{}\t{}\n", plan.identifier, plan.name),
        })
        .collect::<String>();
    Ok(Outcome::raw(text, body))
}

fn show(name: &str) -> Result<Outcome> {
    let runtime = crate::host::runtime()?;
    let detail = ops::plan::show(&runtime, name)?;
    let mut body = plan_edit::doc_as_json(&detail.doc)?;
    body["source"] = serde_json::json!(detail.source);
    Ok(Outcome::raw(detail.yaml, body).with_note(format!("source: {}", detail.source)))
}

pub fn validate(name_or_path: &str) -> Result<Outcome> {
    let runtime = crate::host::runtime()?;
    let verdict = ops::plan::validate(&runtime, name_or_path)?;
    let body = serde_json::to_value(&verdict)?;
    Ok(if verdict.ok {
        Outcome::ok(body)
    } else {
        Outcome::rejected(body)
    })
}

fn migrate(name_or_path: &str) -> Result<Outcome> {
    let path = std::path::Path::new(name_or_path);
    let migrated = if path.exists() {
        ops::plan::migrate_path(path, name_or_path)?
    } else {
        ops::plan::migrate(&crate::host::runtime()?, name_or_path)?
    };
    Ok(migrated_outcome(migrated))
}

pub(crate) fn migrated_outcome(migrated: dive_core::format::Migrated) -> Outcome {
    let mut outcome = Outcome::ok(serde_json::json!({
        "ok": true,
        "savedTo": migrated.path.display().to_string(),
        "from": migrated.from,
        "to": migrated.to,
        "changed": migrated.changed,
        "notes": migrated.notes,
    }));
    if !migrated.changed {
        outcome = outcome.with_note(format!("already at version {}", migrated.to));
    } else if !migrated.notes.is_empty() {
        outcome = outcome.with_note(migrated.notes.join("; "));
    }
    outcome
}

/// The human rendering of a validation verdict.
///
/// Deliberately not the generic [`report`] rendering: a verdict is not an
/// edit, and an invalid plan surfaces as a plain error whose message *is* the
/// problem list — the form that reads best at the end of a shell pipeline.
/// Notes print either way, since "portable but not runnable here" is worth
/// saying and not worth failing over.
fn render_validation(body: &serde_json::Value) -> Result<()> {
    for note in body["notes"].as_array().into_iter().flatten() {
        eprintln!("note: {}", note.as_str().unwrap_or_default());
    }
    let plan = body["plan"].as_str().unwrap_or_default();
    let problems: Vec<&str> = body["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str())
        .collect();
    if !problems.is_empty() {
        bail!(
            "plan '{}' is not valid:\n  - {}",
            plan,
            problems.join("\n  - ")
        );
    }
    eprintln!(
        "ok: '{}' — {} steps",
        plan,
        body["steps"].as_u64().unwrap_or_default()
    );
    Ok(())
}

async fn run_plan(name: &str, document: Option<&str>, inputs: &[String], json: bool) -> Result<()> {
    // `--json` promises machine-parseable stdout, so it suppresses CI
    // annotations (which are stdout workflow commands) even when a mode
    // like DIVE_EVENTS=github is active.
    let annotate = |message: &str| {
        if !json {
            crate::output::annotate_failure(message);
        }
    };
    let runtime = crate::host::runtime()?;
    let loaded = runtime.plan_docs();
    let Some(doc) = loaded.docs.iter().find(|d| d.identifier == name).cloned() else {
        let message = ops::plan::missing(&loaded, name);
        annotate(&message);
        bail!(message);
    };
    // Fail fast: resolve every step tool (sub-plans included) against the
    // loadable catalog before anything runs or connects. At run time a
    // declared-but-unconfigured server is as fatal as an undeclared one.
    let catalog = runtime.tool_catalog(&loaded.docs)?;
    if let Err(error) = callable_from_outside(&catalog.packs, name) {
        annotate(&error.to_string());
        return Err(error);
    }
    let mut check = catalog::resolve_plan_tools_deep(&doc, &loaded.docs, &catalog);
    check.errors.append(&mut check.notes);
    if !check.errors.is_empty() {
        let message = format!(
            "plan '{name}' has unresolvable tools:\n  - {}",
            check.errors.join("\n  - ")
        );
        annotate(&message);
        bail!(message);
    }
    let mut input = resolve_input(document, inputs)?;
    if let Some(schema) = &doc.input_schema {
        dive_core::pipeline::doc::apply_schema_defaults(schema, &mut input);
    }

    if let Err(problems) = validate_input(&doc, &input) {
        eprintln!("plan '{name}' needs inputs:");
        for problem in &problems {
            eprintln!("  - {problem}");
        }
        if let Some(schema) = &doc.input_schema {
            eprintln!("input schema:\n{}", serde_json::to_string_pretty(schema)?);
        }
        annotate(&format!(
            "plan '{name}' needs inputs: {}",
            problems.join("; ")
        ));
        runtime.shutdown().await;
        return Err(SilentExit::code(EXIT_NEEDS_INPUT));
    }

    let store = runtime.store()?;
    // Non-JSON runs stream the solver's answer to stdout as it generates;
    // --json buffers and emits the envelope instead.
    let run = crate::telemetry::RunInfo::plan_run(&doc.identifier)
        .user(runtime.config.user.name.as_deref())
        .input(input.clone());
    let events: Arc<dyn dive_core::EventSink> = crate::output::make_sink(json, !json, run);
    // `ask` steps reach the terminal when there is one. When there isn't
    // (CI, a redirected stdin, machine-readable events), the hook is
    // absent and each `ask` resolves by its declared `whenUnanswered` —
    // the same plan, a declared unattended behaviour.
    let hooks = dive_runtime::runtime::PipelineHooks {
        interlocutor: crate::interlocutor::tty(),
        ..Default::default()
    };
    // Stream each model call to the sink as it lands, so `DIVE_EVENTS=jsonl`
    // carries per-call detail and not just the end-of-run totals.
    runtime.usage.attach_events(events.clone());
    let pipeline = runtime.pipeline_with(&store, events.clone(), hooks).await?;
    let query = format!("Run the '{}' plan", doc.name);
    let finish = doc.finish.clone();
    let result = pipeline
        .run_explicit_in(
            &query,
            doc.steps.clone(),
            finish,
            Some(input),
            doc.pack.as_ref(),
        )
        .await;
    runtime.shutdown().await;
    crate::telemetry::report_plan_result(events.as_ref(), &result);

    // Under DIVE_EVENTS=jsonl the `usage_summary` event carries the report
    // and stderr is one JSON object per line, so a prose summary there would
    // corrupt the feed.
    let jsonl = crate::output::jsonl_events();

    // Drained here rather than inside the pipeline: nested plan calls re-enter
    // `run_explicit` against this same ledger, so this is the only place that
    // knows the whole run is over. Taken before the error branch — a failed
    // run has still spent the tokens, and that is exactly when you want to
    // know how many.
    let usage = runtime.usage.take();
    if !usage.is_empty() {
        events.usage_summary(&usage);
    }

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            // Printed even under --json, unlike the success path: a failed run
            // emits no envelope, so stderr is the only place left to report
            // what it spent — and an expensive run that died is precisely when
            // the number matters. stdout stays empty either way.
            if !jsonl && !usage.is_empty() {
                eprintln!("  {}", usage.summary());
            }
            annotate(&format!("plan '{name}' failed: {err:#}"));
            return Err(err.into());
        }
    };
    let exited_error = matches!(
        &outcome.exit,
        Some(e) if e.status == dive_core::pipeline::ExitStatus::Error
    );
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "answer": (!outcome.answer.is_empty()).then_some(&outcome.answer),
                "output": outcome.structured,
                "plan": doc.identifier,
                "steps_executed": outcome.state.steps_executed(),
                "exit": outcome.exit,
                "usage": (!usage.is_empty()).then_some(&usage),
            }))?
        );
    } else if let Some(exit) = &outcome.exit {
        // Exit-step endings: message to the human, output (if any) to stdout.
        if let Some(structured) = &outcome.structured {
            println!("{}", serde_json::to_string_pretty(structured)?);
        }
        if exited_error {
            eprintln!("✗ {}", exit.message);
        } else if matches!(doc.finish, dive_core::pipeline::Finish::Solver(_)) {
            println!("{}", exit.message);
        } else {
            eprintln!("✓ {}", exit.message);
        }
    } else if let Some(structured) = &outcome.structured {
        println!("{}", serde_json::to_string_pretty(structured)?);
    } else if outcome.answer.is_empty() {
        eprintln!(
            "✓ plan '{}' completed ({} steps)",
            doc.identifier,
            outcome.state.steps_executed()
        );
    } else {
        // Solver output already streamed; just terminate the line.
        println!();
    }
    // stderr, like every other progress line — stdout stays the deliverable.
    // Skipped under --json, where the envelope above already carries it.
    if !json && !jsonl && !usage.is_empty() {
        eprintln!("  {}", usage.summary());
    }
    if exited_error {
        if let Some(exit) = &outcome.exit {
            annotate(&exit.message);
        }
        return Err(SilentExit::code(EXIT_PLAN_ASSERTED));
    }
    Ok(())
}

pub(crate) fn callable_from_outside(packs: &dive_core::packs::PackSet, name: &str) -> Result<()> {
    match packs.refusal(&dive_core::packs::Origin::Outside, name) {
        Some(message) => bail!(message),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_core::packs::{ItemKind, PackItem, PackSet};

    fn packs() -> PackSet {
        let mut set = PackSet::default();
        for (kind, name, public) in [
            (ItemKind::Plan, "describe_file", true),
            (ItemKind::Plan, "scan", false),
            (ItemKind::Tool, "file_info", false),
        ] {
            set.insert(PackItem {
                pack: "example".to_string(),
                kind,
                name: name.to_string(),
                public,
            });
        }
        set
    }

    #[test]
    fn plan_run_refuses_a_private_pack_plan_naming_its_pack() {
        let error = callable_from_outside(&packs(), "example__plan__scan")
            .unwrap_err()
            .to_string();
        assert!(error.contains("'example__plan__scan'"), "{error}");
        assert!(error.contains("pack 'example'"), "{error}");
    }

    #[test]
    fn plan_run_accepts_public_pack_plans_and_user_plans() {
        assert!(callable_from_outside(&packs(), "example__plan__describe_file").is_ok());
        assert!(callable_from_outside(&packs(), "sprint_report").is_ok());
    }
}
