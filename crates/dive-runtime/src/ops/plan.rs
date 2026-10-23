use crate::runtime::Runtime;
use anyhow::{bail, Result};
use dive_core::format::Migrated;
use dive_core::packs::Origin;
use dive_core::pipeline::authoring;
use dive_core::pipeline::catalog;
use dive_core::pipeline::doc::{parse_plan_doc, LoadedPlans, PlanDoc};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanSummary {
    pub identifier: String,
    pub name: String,
    pub description: String,
    pub steps: usize,
    pub path: Option<String>,
    pub source: String,
    #[serde(skip)]
    pub pack: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedPlan {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenPlan {
    pub identifier: String,
    pub missing_servers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanList {
    pub plans: Vec<PlanSummary>,
    pub skipped: Vec<SkippedPlan>,
    pub hidden: Vec<HiddenPlan>,
}

impl PlanList {
    pub fn is_empty(&self) -> bool {
        self.plans.is_empty() && self.skipped.is_empty()
    }
}

impl From<&LoadedPlans> for PlanList {
    fn from(loaded: &LoadedPlans) -> Self {
        Self {
            plans: loaded
                .docs
                .iter()
                .map(|doc| PlanSummary {
                    identifier: doc.identifier.clone(),
                    name: doc.name.clone(),
                    description: doc.description.clone(),
                    steps: doc.steps.len(),
                    path: doc.path.as_ref().map(|p| p.display().to_string()),
                    source: doc.source_label(),
                    pack: doc.pack.as_ref().map(|source| source.describe()),
                })
                .collect(),
            skipped: loaded
                .skipped
                .iter()
                .map(|error| SkippedPlan {
                    path: error.path().to_string(),
                    reason: error.to_string(),
                })
                .collect(),
            hidden: loaded
                .hidden
                .iter()
                .map(|hidden| HiddenPlan {
                    identifier: hidden.identifier.clone(),
                    missing_servers: hidden.missing_servers.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanDetail {
    pub doc: PlanDoc,
    pub yaml: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanValidation {
    pub plan: String,
    pub steps: usize,
    pub ok: bool,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

pub fn list(runtime: &Runtime) -> PlanList {
    let mut loaded = runtime.plan_docs();
    let packs = runtime.packs.pack_set();
    loaded
        .docs
        .retain(|doc| packs.shows(&Origin::Outside, &doc.identifier));
    PlanList::from(&loaded)
}

pub fn show(runtime: &Runtime, name_or_path: &str) -> Result<PlanDetail> {
    let (doc, _loaded) = resolve(runtime, name_or_path)?;
    Ok(PlanDetail {
        yaml: authoring::to_yaml(&doc)?,
        source: doc.source_label(),
        doc,
    })
}

pub fn resolve(runtime: &Runtime, name_or_path: &str) -> Result<(PlanDoc, LoadedPlans)> {
    let loaded = runtime.plan_docs();
    let path = Path::new(name_or_path);
    if path.exists() {
        return Ok((parse_plan_doc(path)?, loaded));
    }
    if let Some(doc) = loaded.docs.iter().find(|d| d.identifier == name_or_path) {
        return Ok((doc.clone(), loaded));
    }
    if let Some(found) = runtime.find_plan_file(name_or_path) {
        return Ok((parse_plan_doc(&found)?, loaded));
    }
    bail!(missing(&loaded, name_or_path))
}

pub fn missing(loaded: &LoadedPlans, name: &str) -> String {
    if let Some(reason) = loaded.hidden_reason(name) {
        return reason;
    }
    match loaded.skip_reason(name) {
        Some(reason) => format!("plan '{name}' failed to load — {reason}"),
        None => format!("no plan named '{name}' (see `graph plan list`)"),
    }
}

pub fn validate(runtime: &Runtime, name_or_path: &str) -> Result<PlanValidation> {
    let (doc, loaded) = resolve(runtime, name_or_path)?;
    let mut problems = authoring::static_problems(&doc);
    for problem in authoring::model_kind_problems(&doc, &|role| runtime.model_kind_of(role)) {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    let catalog = runtime.tool_catalog(&loaded.docs)?;
    let check = catalog::resolve_plan_tools_deep(&doc, &loaded.docs, &catalog);
    for problem in check.errors {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    Ok(PlanValidation {
        plan: doc.identifier.clone(),
        steps: doc.steps.len(),
        ok: problems.is_empty(),
        problems,
        notes: check.notes,
    })
}

pub fn migrate(runtime: &Runtime, name_or_path: &str) -> Result<Migrated> {
    let path = Path::new(name_or_path);
    let path = if path.exists() {
        path.to_path_buf()
    } else {
        runtime.find_plan_file(name_or_path).ok_or_else(|| {
            anyhow::anyhow!("no plan file for '{name_or_path}' under [plans].paths")
        })?
    };
    migrate_path(&path, name_or_path)
}

pub fn migrate_path(path: &Path, label: &str) -> Result<Migrated> {
    let check = |value: &serde_yaml::Value| -> Result<(), String> {
        let yaml = serde_yaml::to_string(value).map_err(|e| e.to_string())?;
        dive_core::pipeline::doc::parse_plan_source(&yaml, label)
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    dive_core::format::migrate_file(dive_core::format::Kind::Plan, path, &check)
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::InstalledPacks;
    use crate::runtime::RuntimeOptions;
    use dive_core::pipeline::doc::DocError;
    use dive_llm::ModelRouter;
    use serde_json::json;
    use std::collections::HashMap;

    fn runtime(plans: &Path) -> Runtime {
        let mut config = dive_config::Config::default();
        config.tools.paths = Vec::new();
        config.plans.paths = vec![plans.to_path_buf()];
        let router = ModelRouter::with_providers(HashMap::new(), Default::default());
        let options = RuntimeOptions {
            storage_backend: Some("memory".to_string()),
            capture_content: false,
        };
        Runtime::with_router(config, InstalledPacks::default(), options, router).unwrap()
    }

    const DEMO: &str = "identifier: demo\nname: Demo\ndescription: demo plan\nsteps:\n  - id: E1\n    tool_name: builtin__reshape\n    input: { value: x, template: x }\n";

    #[test]
    fn list_carries_user_plans_with_the_cli_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("demo.yaml"), DEMO).unwrap();
        std::fs::write(dir.path().join("broken.yaml"), "identifier: broken\n").unwrap();
        let listed = list(&runtime(dir.path()));
        let demo = listed
            .plans
            .iter()
            .find(|plan| plan.identifier == "demo")
            .unwrap();
        assert_eq!(demo.steps, 1);
        assert_eq!(demo.pack, None);
        let value = serde_json::to_value(&listed).unwrap();
        let entry = value["plans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|plan| plan["identifier"] == "demo")
            .unwrap();
        assert_eq!(
            entry.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "description",
                "identifier",
                "name",
                "path",
                "source",
                "steps"
            ]
        );
        assert!(value["skipped"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("broken.yaml"));
    }

    #[test]
    fn hidden_plans_serialize_their_missing_servers_in_camel_case() {
        let loaded = LoadedPlans {
            docs: Vec::new(),
            skipped: vec![DocError::Invalid {
                path: "/plans/broken.yaml".to_string(),
                message: "plan has no steps".to_string(),
            }],
            hidden: vec![dive_core::pipeline::doc::HiddenPlan {
                identifier: "needs_linear".to_string(),
                missing_servers: vec!["linear".to_string()],
            }],
        };
        let value = serde_json::to_value(PlanList::from(&loaded)).unwrap();
        assert_eq!(value["hidden"][0]["missingServers"], json!(["linear"]));
        assert_eq!(value["skipped"][0]["path"], json!("/plans/broken.yaml"));
        assert_eq!(
            missing(&loaded, "needs_linear"),
            loaded.hidden_reason("needs_linear").unwrap()
        );
        assert_eq!(
            missing(&loaded, "broken"),
            format!("plan 'broken' failed to load — {}", loaded.skipped[0])
        );
        assert_eq!(
            missing(&loaded, "nothing"),
            "no plan named 'nothing' (see `graph plan list`)"
        );
    }

    #[test]
    fn show_and_validate_resolve_by_identifier_and_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("demo.yaml");
        std::fs::write(&file, DEMO).unwrap();
        let runtime = runtime(dir.path());

        let shown = show(&runtime, "demo").unwrap();
        assert_eq!(shown.doc.identifier, "demo");
        assert_eq!(shown.source, file.display().to_string());
        assert!(shown.yaml.contains("identifier: demo"));

        let verdict = validate(&runtime, &file.display().to_string()).unwrap();
        assert_eq!(verdict.plan, "demo");
        assert_eq!(verdict.steps, 1);
        assert_eq!(verdict.ok, verdict.problems.is_empty());

        let error = show(&runtime, "absent").unwrap_err().to_string();
        assert_eq!(error, "no plan named 'absent' (see `graph plan list`)");
    }

    #[test]
    fn validate_reports_unresolvable_tools_as_problems() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bad.yaml"),
            DEMO.replace("builtin__reshape", "nowhere__tool"),
        )
        .unwrap();
        let verdict = validate(&runtime(dir.path()), "demo").unwrap();
        assert!(!verdict.ok);
        assert!(!verdict.problems.is_empty());
        let value = serde_json::to_value(&verdict).unwrap();
        assert_eq!(
            value.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["notes", "ok", "plan", "problems", "steps"]
        );
    }

    #[test]
    fn migrate_stamps_the_current_version_and_refuses_unknown_plans() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("demo.yaml"), DEMO).unwrap();
        let runtime = runtime(dir.path());
        let migrated = migrate(&runtime, "demo").unwrap();
        assert_eq!(migrated.to, dive_core::format::PLAN_FORMAT);
        assert_eq!(migrated.path, dir.path().join("demo.yaml"));
        let again = migrate(&runtime, "demo").unwrap();
        assert!(!again.changed);
        let error = migrate(&runtime, "absent").unwrap_err().to_string();
        assert_eq!(error, "no plan file for 'absent' under [plans].paths");
    }
}
