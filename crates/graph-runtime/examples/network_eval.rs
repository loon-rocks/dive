use graph_runtime::runtime::{RuntimeOptions, Workspace};
use graph_runtime::workbench::{
    GateKind, NetworkView, Start, UiDecision, Workbench, WorkbenchEvent,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Deserialize, Clone)]
struct Goal {
    id: String,
    request: String,
    #[serde(default)]
    answers: Vec<String>,
    #[serde(default)]
    test: Option<String>,
}

#[derive(Default)]
struct Seen {
    timeline: Vec<String>,
    asks: usize,
    tools: BTreeMap<String, usize>,
    usage: Vec<String>,
    network: Option<NetworkView>,
    sketched_at: Option<f64>,
    drafted_at: Option<f64>,
    tested_at: Option<f64>,
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn parse_tokens(text: &str) -> f64 {
    let text = text.trim();
    let (number, scale) = match text.chars().last() {
        Some('k') => (&text[..text.len() - 1], 1e3),
        Some('M') => (&text[..text.len() - 1], 1e6),
        _ => (text, 1.0),
    };
    number.parse::<f64>().unwrap_or(0.0) * scale
}

fn usage_totals(lines: &[String]) -> Value {
    let (mut calls, mut input, mut output, mut cost) = (0.0, 0.0, 0.0, 0.0);
    for line in lines {
        for part in line.split('·') {
            let part = part.trim();
            if let Some(n) = part
                .strip_suffix(" calls")
                .or_else(|| part.strip_suffix(" call"))
            {
                calls += n.trim().parse::<f64>().unwrap_or(0.0);
            } else if let Some((i, o)) = part.split_once(" in / ") {
                input += parse_tokens(i);
                output += parse_tokens(o.split(' ').next().unwrap_or("0"));
            } else if let Some(c) = part.strip_prefix('$') {
                cost += c.parse::<f64>().unwrap_or(0.0);
            }
        }
    }
    json!({"calls": calls, "input_tokens": input, "output_tokens": output, "cost_usd": cost})
}

fn next_message(
    goal: &Goal,
    view: &NetworkView,
    reply: &str,
    answered: &mut usize,
    build_sent: &mut bool,
    test_sent: &mut usize,
) -> Option<String> {
    let drafted = view.nodes.iter().filter(|n| n.yaml.is_some()).count();
    let missing = view
        .nodes
        .iter()
        .filter(|n| n.sketched && n.yaml.is_none())
        .count();
    if view.tested {
        return None;
    }
    if view.restate.is_some() && drafted == 0 && !*build_sent {
        *build_sent = true;
        return Some("Yes, build it.".to_string());
    }
    if view.restate.is_none() {
        let lower = reply.to_lowercase();
        if lower.contains("yes, no, or refine")
            || lower.contains("yes / no")
            || lower.contains("confirm")
        {
            return Some("Yes.".to_string());
        }
        if let Some(answer) = goal.answers.get(*answered) {
            *answered += 1;
            return Some(answer.clone());
        }
        return Some("Go with your guess.".to_string());
    }
    if drafted > 0 && missing == 0 && *test_sent < 2 {
        *test_sent += 1;
        return Some(match &goal.test {
            Some(message) => format!("Test it with: {message}"),
            None => "Test it.".to_string(),
        });
    }
    Some("Go with your guess and keep going.".to_string())
}

fn turn_limit() -> u64 {
    std::env::var("EVAL_TURN_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(240)
}

async fn run_goal(root: PathBuf, goal: Goal, max_turns: usize) -> anyhow::Result<Value> {
    let scratch = tempfile::tempdir()?;
    let mut loaded = graph_runtime::runtime::load_config(&Workspace::new(&root))?;
    loaded.config.settings.data_dir = scratch.path().join("data");
    loaded
        .config
        .tools
        .paths
        .insert(0, scratch.path().join("tools"));
    loaded
        .config
        .plans
        .paths
        .insert(0, scratch.path().join("plans"));
    if let Ok(roles) = std::env::var("EVAL_ROLES") {
        let mut map: BTreeMap<String, graph_config::ModelChoice> = loaded
            .config
            .models
            .iter()
            .map(|(name, choice)| (name.to_string(), choice.clone()))
            .collect();
        for spec in roles.split(',').filter(|s| !s.is_empty()) {
            let (role, model) = spec
                .split_once('=')
                .expect("EVAL_ROLES is role=provider:model,...");
            let (provider, model) = model.split_once(':').expect("provider:model");
            map.insert(
                role.to_string(),
                graph_config::ModelChoice {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                },
            );
        }
        loaded.config.models = graph_config::ModelRoles::new(map);
    }
    let packs = graph_runtime::runtime::load_packs(&loaded)?;
    let runtime = graph_runtime::runtime::Runtime::with_packs(
        loaded.config,
        packs,
        RuntimeOptions::default(),
    )?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkbenchEvent>();
    let started = Instant::now();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let collector = {
        let seen = seen.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let at = started.elapsed().as_secs_f64();
                let mut seen = seen.lock().unwrap();
                let (_, line) = event.log_line();
                if !line.starts_with("network:") && !line.starts_with("agent delta") {
                    seen.timeline.push(format!(
                        "{at:7.1} {}",
                        line.chars().take(200).collect::<String>()
                    ));
                }
                match event {
                    WorkbenchEvent::GateAsk { kind, reply, .. } => {
                        seen.asks += 1;
                        let result = match kind {
                            GateKind::Ask { default, .. } => default.unwrap_or_else(|| json!({})),
                            _ => json!({}),
                        };
                        let _ = reply.send(UiDecision::Skip { result });
                    }
                    WorkbenchEvent::AgentToolStarted(name) => {
                        *seen.tools.entry(name).or_default() += 1;
                    }
                    WorkbenchEvent::RunUsage(line) => seen.usage.push(line),
                    WorkbenchEvent::NetworkChanged(view) => {
                        if view.restate.is_some() && seen.sketched_at.is_none() {
                            seen.sketched_at = Some(at);
                        }
                        let drafted = view.nodes.iter().filter(|n| n.yaml.is_some()).count();
                        let missing = view
                            .nodes
                            .iter()
                            .filter(|n| n.sketched && n.yaml.is_none())
                            .count();
                        if drafted > 0 && missing == 0 && seen.drafted_at.is_none() {
                            seen.drafted_at = Some(at);
                        }
                        if view.tested && seen.tested_at.is_none() {
                            seen.tested_at = Some(at);
                        }
                        seen.network = Some(*view);
                    }
                    _ => {}
                }
            }
        })
    };
    let workbench = Workbench::open(&runtime, Start::Agent(None), &root, tx).await?;
    let mut transcript = Vec::new();
    let mut message = Some(goal.request.clone());
    let (mut answered, mut build_sent, mut test_sent) = (0, false, 0);
    let mut build_started = None;
    let mut turns = Vec::new();
    let mut error = None;
    for _ in 0..max_turns {
        let Some(text) = message.take() else { break };
        if text.starts_with("Yes, build it") {
            build_started = Some(started.elapsed().as_secs_f64());
        }
        let turn_started = Instant::now();
        let reply = match tokio::time::timeout(
            std::time::Duration::from_secs(turn_limit()),
            workbench.send_message(text.clone()),
        )
        .await
        {
            Ok(reply) => reply,
            Err(_) => Err(format!("turn timed out after {}s", turn_limit())),
        };
        let seconds = turn_started.elapsed().as_secs_f64();
        let reply_text = match &reply {
            Ok(text) => text.clone(),
            Err(problem) => {
                error = Some(problem.clone());
                problem.clone()
            }
        };
        turns.push(
            json!({"user": text, "seconds": (seconds * 10.0).round() / 10.0, "reply": reply_text}),
        );
        transcript.push(reply_text.clone());
        if reply.is_err() {
            break;
        }
        let view = workbench.network();
        let finished = seen.lock().unwrap().drafted_at.is_some() && view.is_empty();
        if finished {
            break;
        }
        message = next_message(
            &goal,
            &view,
            &reply_text,
            &mut answered,
            &mut build_sent,
            &mut test_sent,
        );
    }
    let total = started.elapsed().as_secs_f64();
    drop(workbench);
    runtime.shutdown().await;
    drop(runtime);
    let _ = collector.await;
    let seen = seen.lock().unwrap();
    let view = seen.network.clone().unwrap_or_default();
    let round = |x: Option<f64>| x.map(|x| (x * 10.0).round() / 10.0);
    let build_seconds = match (build_started, seen.tested_at.or(seen.drafted_at)) {
        (Some(start), Some(end)) if end > start => Some(end - start),
        _ => None,
    };
    Ok(json!({
        "id": goal.id,
        "total_seconds": round(Some(total)),
        "sketched_at": round(seen.sketched_at),
        "build_started_at": round(build_started),
        "drafted_at": round(seen.drafted_at),
        "tested_at": round(seen.tested_at),
        "build_seconds": round(build_seconds),
        "tested": view.tested,
        "nodes": view.nodes.iter().map(|n| json!({
            "name": n.name, "kind": n.kind.label(), "status": n.status.label(),
            "model": n.model, "problems": n.problems, "drafted": n.yaml.is_some(), "yaml": n.yaml,
        })).collect::<Vec<_>>(),
        "edges": view.edges.iter().map(|e| format!("{} {} {}", e.from, e.kind.label(), e.to)).collect::<Vec<_>>(),
        "tests": view.tests.iter().map(|t| json!({"passed": t.passed, "ran": t.ran, "answer": t.answer})).collect::<Vec<_>>(),
        "restate": view.restate.as_ref().map(|r| json!({"outcome": r.outcome, "success": r.success, "constraint": r.constraint, "out_of_scope": r.out_of_scope})),
        "usage": usage_totals(&seen.usage),
        "tools": seen.tools,
        "asks": seen.asks,
        "turns": turns,
        "timeline": seen.timeline,
        "error": error,
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let goals_path =
        arg(&args, "--goals").unwrap_or_else(|| ".graph/evals/network/goals.yaml".into());
    let out = arg(&args, "--out").unwrap_or_else(|| "/tmp/network-eval/results.json".into());
    let only = arg(&args, "--only");
    let workers: usize = arg(&args, "--workers")
        .and_then(|w| w.parse().ok())
        .unwrap_or(3);
    let max_turns: usize = arg(&args, "--turns")
        .and_then(|w| w.parse().ok())
        .unwrap_or(12);
    let root = std::env::current_dir()?;
    let goals: Vec<Goal> = serde_yaml::from_str(&std::fs::read_to_string(&goals_path)?)?;
    let goals: Vec<Goal> = goals
        .into_iter()
        .filter(|g| {
            only.as_deref()
                .is_none_or(|only| only.split(',').any(|o| o == g.id))
        })
        .collect();
    let mut results = Vec::new();
    if workers > 1 && goals.len() > 1 {
        let exe = std::env::current_exe()?;
        let scratch = tempfile::tempdir()?;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(workers));
        let mut handles = Vec::new();
        for goal in &goals {
            let permit = semaphore.clone().acquire_owned().await?;
            let part = scratch.path().join(format!("{}.json", goal.id));
            let mut command = tokio::process::Command::new(&exe);
            command
                .args(["--goals", &goals_path, "--only", &goal.id, "--workers", "1"])
                .args(["--turns", &max_turns.to_string()])
                .arg("--out")
                .arg(&part)
                .stdout(std::process::Stdio::null());
            let mut child = command.spawn()?;
            handles.push(tokio::spawn(async move {
                let _ = child.wait().await;
                drop(permit);
                part
            }));
        }
        for handle in handles {
            let part = handle.await?;
            if let Ok(text) = std::fs::read_to_string(&part) {
                if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) {
                    results.extend(items);
                }
            }
        }
    } else {
        for goal in goals {
            let id = goal.id.clone();
            let result = run_goal(root.clone(), goal, max_turns)
                .await
                .unwrap_or_else(|error| json!({"id": id, "error": format!("{error:#}")}));
            eprintln!(
                "{:<22} build {:>6}s  total {:>6}s  tested {:<5}  nodes {}  calls {}  out {}  {}",
                result["id"].as_str().unwrap_or_default(),
                result["build_seconds"],
                result["total_seconds"],
                result["tested"],
                result["nodes"].as_array().map(|n| n.len()).unwrap_or(0),
                result["usage"]["calls"],
                result["usage"]["output_tokens"],
                result["error"].as_str().unwrap_or("")
            );
            results.push(result);
        }
    }
    if let Some(parent) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, serde_json::to_string_pretty(&results)?)?;
    eprintln!("wrote {out}");
    Ok(())
}
