use anyhow::{bail, Result};
use dive_core::format::Migrated;
use dive_core::toolbox::AgentToolbox;
use dive_core::user_tools::ToolDocs;
use dive_core::{ToolDef, ToolRegistry};
use dive_mcp::NAMESPACE_SEPARATOR;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub fn source_of(name: &str) -> &str {
    if dive_core::pipeline::is_control_step(name) {
        return "(control)";
    }
    name.split_once(NAMESPACE_SEPARATOR)
        .map_or("(core)", |(source, _)| source)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEntry {
    pub name: String,
    pub source: String,
    pub description: String,
    pub read_only: Option<bool>,
    pub has_output_schema: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceCount {
    pub source: String,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolListing {
    pub tools: Vec<ToolEntry>,
    pub count: usize,
    pub sources: Vec<SourceCount>,
    #[serde(skip)]
    pub defs: Vec<ToolDef>,
}

impl ToolListing {
    pub fn new(defs: Vec<ToolDef>) -> Self {
        Self::labeled(defs, &BTreeMap::new())
    }

    pub fn labeled(defs: Vec<ToolDef>, labels: &BTreeMap<String, String>) -> Self {
        let label = |source: &str| labels.get(source).cloned();
        let mut sources: Vec<SourceCount> = Vec::new();
        for def in &defs {
            let source = source_of(&def.name);
            match sources.iter_mut().find(|entry| entry.source == source) {
                Some(entry) => entry.count += 1,
                None => sources.push(SourceCount {
                    source: source.to_string(),
                    count: 1,
                    pack: label(source),
                }),
            }
        }
        Self {
            tools: defs
                .iter()
                .map(|def| ToolEntry {
                    name: def.name.clone(),
                    source: source_of(&def.name).to_string(),
                    description: def.description.clone(),
                    read_only: def.read_only,
                    has_output_schema: def.output_schema.is_some(),
                    pack: label(source_of(&def.name)),
                })
                .collect(),
            count: defs.len(),
            sources,
            defs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDetail {
    pub name: String,
    pub source: String,
    pub description: String,
    pub read_only: Option<bool>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub output_example: Option<Value>,
    #[serde(skip)]
    pub origin: Option<String>,
}

impl From<&ToolDef> for ToolDetail {
    fn from(def: &ToolDef) -> Self {
        Self {
            name: def.name.clone(),
            source: source_of(&def.name).to_string(),
            description: def.description.clone(),
            read_only: def.read_only,
            input_schema: def.input_schema.clone(),
            output_schema: def.output_schema.clone(),
            output_example: def.output_example.clone(),
            origin: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTest {
    pub tool: String,
    pub is_error: bool,
    pub result: Value,
}

pub async fn catalog(registry: &(dyn ToolRegistry + Send + Sync)) -> Result<Vec<ToolDef>> {
    let mut defs = registry.tools().await?;
    defs.extend(dive_core::pipeline::control_step_defs());
    Ok(defs)
}

pub fn pack_labels(docs: &ToolDocs) -> BTreeMap<String, String> {
    docs.packs
        .iter()
        .map(|(source, _)| (source.name.clone(), source.describe()))
        .collect()
}

pub async fn list(
    registry: &(dyn ToolRegistry + Send + Sync),
    labels: &BTreeMap<String, String>,
) -> Result<ToolListing> {
    Ok(ToolListing::labeled(catalog(registry).await?, labels))
}

pub fn tool_source(docs: &ToolDocs, name: &str) -> Option<String> {
    if let Some(source) = docs.pack_source(name) {
        return Some(source.describe());
    }
    let (list, bare) = match name.split_once("__")? {
        ("builtin", bare) => (&docs.builtins, bare),
        ("user", bare) => (&docs.user, bare),
        _ => return None,
    };
    let doc = list.iter().find(|doc| doc.name == bare)?;
    Some(match &doc.path {
        Some(path) => path.display().to_string(),
        None => "built-in".to_string(),
    })
}

pub async fn show(
    registry: &(dyn ToolRegistry + Send + Sync),
    docs: &ToolDocs,
    name: &str,
) -> Result<ToolDetail> {
    let defs = catalog(registry).await?;
    let Some(def) = defs.iter().find(|d| d.name == name) else {
        bail!("unknown tool: {name}");
    };
    let mut detail = ToolDetail::from(def);
    if let Some(origin) = tool_source(docs, name) {
        detail.source = origin.clone();
        detail.origin = Some(origin);
    }
    Ok(detail)
}

pub fn testable(registry: &AgentToolbox, name: &str) -> Result<()> {
    if let Some(message) = registry.refusal(name) {
        bail!(message);
    }
    if dive_core::pipeline::is_control_step(name) {
        bail!(
            "'{name}' is a control step, not an invokable tool — it is evaluated \
             by the plan executor. Add it to a plan (dive plan step add) and run \
             the plan; `dive tool show {name}` describes its input."
        );
    }
    Ok(())
}

pub async fn test(registry: &AgentToolbox, name: &str, input: Value) -> Result<ToolTest> {
    testable(registry, name)?;
    let invoked = registry.invoke(name, input).await?;
    Ok(ToolTest {
        tool: name.to_string(),
        is_error: invoked.is_error,
        result: invoked.result,
    })
}

pub fn migrate(path: &Path) -> Result<Migrated> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let check = |value: &serde_yaml::Value| -> Result<(), String> {
        let yaml = serde_yaml::to_string(value).map_err(|e| e.to_string())?;
        dive_core::user_tools::parse_tool_source(&yaml).map(|_| ())
    };
    dive_core::format::migrate_file(dive_core::format::Kind::Tool, path, &check)
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str, description: &str, read_only: Option<bool>) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: json!({}),
            output_schema: None,
            output_example: None,
            read_only,
            file_input: Default::default(),
        }
    }

    #[test]
    fn sources_group_namespaces_core_names_and_control_steps() {
        assert_eq!(source_of("linear__list_issues"), "linear");
        assert_eq!(source_of("plan_and_execute"), "(core)");
        assert_eq!(source_of("route"), "(control)");
    }

    #[test]
    fn a_labeled_listing_marks_pack_entries_and_their_rollup() {
        let defs = vec![
            def("github__tool__pr", "PRs.", Some(true)),
            def("linear__list", "List.", None),
        ];
        let labels = BTreeMap::from([("github".to_string(), "pack github v1".to_string())]);
        let value = serde_json::to_value(ToolListing::labeled(defs, &labels)).unwrap();
        assert_eq!(value["tools"][0]["pack"], json!("pack github v1"));
        assert!(value["tools"][1].get("pack").is_none());
        assert_eq!(
            value["sources"],
            json!([
                {"source": "github", "count": 1, "pack": "pack github v1"},
                {"source": "linear", "count": 1},
            ])
        );
        assert_eq!(value["count"], 2);
    }

    #[test]
    fn detail_keeps_absent_optionals_as_null() {
        assert_eq!(
            serde_json::to_value(ToolDetail::from(&def("s__t", "T.", None))).unwrap(),
            json!({
                "name": "s__t",
                "source": "s",
                "description": "T.",
                "readOnly": null,
                "inputSchema": {},
                "outputSchema": null,
                "outputExample": null,
            })
        );
    }

    #[test]
    fn tool_source_names_the_overriding_file_or_the_pack() {
        let builtins = dive_core::user_tools::load_builtin_tools().unwrap();
        let pack_tool = dive_core::user_tools::parse_tool_source(
            "name: file_info\ndescription: d\nkind: exec\ncommand: wc\n",
        )
        .unwrap();
        let docs = ToolDocs {
            builtins: builtins.clone(),
            user: Vec::new(),
            packs: vec![(
                dive_core::packs::PackSource {
                    name: "example".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    layer: dive_config::pack::LayerKind::Project,
                    dir: std::path::PathBuf::from(".dive/packs/example"),
                },
                vec![pack_tool],
            )],
        };
        assert_eq!(
            tool_source(&docs, "example__tool__file_info").as_deref(),
            Some("pack example v1.0.0 (project)")
        );
        assert_eq!(tool_source(&docs, "example__tool__missing"), None);
        assert_eq!(
            tool_source(&docs, "builtin__reshape").as_deref(),
            Some("built-in")
        );
        let mut mine = builtins
            .iter()
            .find(|d| d.name == "reshape")
            .unwrap()
            .clone();
        mine.path = Some(std::path::PathBuf::from("/tools/builtin/reshape.yaml"));
        let overridden = ToolDocs {
            builtins: dive_core::user_tools::apply_tool_overrides(builtins, vec![mine]).unwrap(),
            user: Vec::new(),
            packs: Vec::new(),
        };
        assert_eq!(
            tool_source(&overridden, "builtin__reshape").as_deref(),
            Some("/tools/builtin/reshape.yaml")
        );
        assert_eq!(tool_source(&docs, "linear__get_issue"), None);
    }

    #[test]
    fn migrate_refuses_a_missing_file_and_stamps_an_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.yaml");
        let error = migrate(&path).unwrap_err().to_string();
        assert!(error.ends_with("does not exist"), "{error}");
        std::fs::write(&path, "name: t\ndescription: d\nkind: exec\ncommand: wc\n").unwrap();
        let migrated = migrate(&path).unwrap();
        assert_eq!(migrated.to, dive_core::format::TOOL_FORMAT);
        assert!(migrated.changed);
    }

    #[test]
    fn show_json_carries_the_whole_definition() {
        let mut with_schema = def("linear__list_issues", "List issues.", Some(true));
        with_schema.input_schema =
            json!({"type": "object", "properties": {"team": {"type": "string"}}});
        with_schema.output_schema = Some(json!({"type": "object"}));
        with_schema.output_example = Some(json!({"issues": []}));
        assert_eq!(
            serde_json::to_value(ToolDetail::from(&with_schema)).unwrap(),
            json!({
                "name": "linear__list_issues",
                "source": "linear",
                "description": "List issues.",
                "readOnly": true,
                "inputSchema": {"type": "object", "properties": {"team": {"type": "string"}}},
                "outputSchema": {"type": "object"},
                "outputExample": {"issues": []},
            })
        );
    }

    #[test]
    fn show_json_keeps_absent_optionals_as_null() {
        assert_eq!(
            serde_json::to_value(ToolDetail::from(&def(
                "plan_and_execute",
                "Plan and execute.",
                None
            )))
            .unwrap(),
            json!({
                "name": "plan_and_execute",
                "source": "(core)",
                "description": "Plan and execute.",
                "readOnly": null,
                "inputSchema": {},
                "outputSchema": null,
                "outputExample": null,
            })
        );
    }

    #[test]
    fn json_carries_namespaced_names_and_a_source_rollup() {
        let defs = vec![
            def("everything__echo", "Echoes back the input.", None),
            def("everything__get-sum", "Adds two numbers.", Some(true)),
            def("plan_and_execute", "Plan and execute a task.", None),
        ];
        assert_eq!(
            serde_json::to_value(ToolListing::new(defs)).unwrap(),
            json!({
                "tools": [
                    {
                        "name": "everything__echo",
                        "source": "everything",
                        "description": "Echoes back the input.",
                        "readOnly": null,
                        "hasOutputSchema": false,
                    },
                    {
                        "name": "everything__get-sum",
                        "source": "everything",
                        "description": "Adds two numbers.",
                        "readOnly": true,
                        "hasOutputSchema": false,
                    },
                    {
                        "name": "plan_and_execute",
                        "source": "(core)",
                        "description": "Plan and execute a task.",
                        "readOnly": null,
                        "hasOutputSchema": false,
                    },
                ],
                "count": 3,
                "sources": [
                    {"source": "everything", "count": 2},
                    {"source": "(core)", "count": 1},
                ],
            })
        );
    }

    #[test]
    fn json_keeps_multi_line_descriptions_whole() {
        let defs = vec![def("s__t", "First line.\nSecond line.", None)];
        let listing = serde_json::to_value(ToolListing::new(defs)).unwrap();
        assert_eq!(
            listing["tools"][0]["description"],
            "First line.\nSecond line."
        );
    }

    #[test]
    fn json_reports_an_empty_catalog_as_a_valid_envelope() {
        assert_eq!(
            serde_json::to_value(ToolListing::new(Vec::new())).unwrap(),
            json!({"tools": [], "count": 0, "sources": []})
        );
    }

    #[test]
    fn json_flags_a_declared_output_schema() {
        let mut with_schema = def("s__t", "Desc.", None);
        with_schema.output_schema = Some(json!({"type": "object"}));
        let listing = serde_json::to_value(ToolListing::new(vec![with_schema])).unwrap();
        assert_eq!(listing["tools"][0]["hasOutputSchema"], true);
    }
}
