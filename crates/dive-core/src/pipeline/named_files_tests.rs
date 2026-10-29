use super::*;
use crate::files::memory::Memory;
use crate::files::named::FileSpace;
use crate::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde_json::json;

struct Writer;

#[async_trait]
impl ToolRegistry for Writer {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(Vec::new())
    }

    async fn invoke(&self, _name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let content = input["content"].as_str().unwrap_or_default();
        if let Some(path) = input["filename"].as_str() {
            std::fs::write(path, content).unwrap();
        }
        if let Some(dir) = input["dir"].as_str() {
            for page in 1..=3 {
                std::fs::write(format!("{dir}/page-{page}.png"), format!("{content}{page}"))
                    .unwrap();
            }
        }
        Ok(ToolOutcome {
            result: json!({"text": "### Result\n- [saved](./snapshot.json)"}),
            is_error: false,
        })
    }
}

fn pipeline(workspace: &std::path::Path) -> Pipeline {
    let mut pipeline = super::tests::pipeline_for(Arc::new(Writer));
    pipeline.files = Some(FileSpace {
        store: Arc::new(Memory::default()),
        dir: workspace.join(".dive/files"),
    });
    pipeline
}

fn steps(yaml: &str) -> Plan {
    serde_yaml::from_str(yaml).unwrap()
}

#[tokio::test]
async fn a_step_declares_a_file_and_a_later_step_reads_it_by_name() {
    let workspace = tempfile::tempdir().unwrap();
    let plan = steps(
        r#"
- id: shot
  tool_name: t__evaluate
  files:
    snapshot: { name: snapshot.json }
  input:
    content: '{"a": 1}'
    filename: "{{files.snapshot.path}}"
- id: echo
  tool_name: t__evaluate
  input:
    content: "{{shot.files.snapshot.ref}}"
"#,
    );
    let mut output = Map::new();
    output.insert("ref".into(), json!("{{shot.files.snapshot.ref}}"));
    output.insert("mime".into(), json!("{{shot.files.snapshot.mime}}"));
    output.insert("first".into(), json!("{{shot.files.0.ref}}"));
    let outcome = pipeline(workspace.path())
        .run_explicit("q", plan, Finish::Output(output), None)
        .await
        .unwrap();
    let out = outcome.structured.unwrap();
    assert!(out["ref"].as_str().unwrap().ends_with(".json"), "{out}");
    assert_eq!(out["mime"], "application/json");
    assert_eq!(out["ref"], out["first"]);
    let tmp = workspace.path().join(".dive/files/tmp");
    assert_eq!(std::fs::read_dir(tmp).unwrap().count(), 0);
}

#[tokio::test]
async fn parallel_map_items_get_their_own_directories_and_refs() {
    let workspace = tempfile::tempdir().unwrap();
    let plan = steps(
        r#"
- id: saved
  tool_name: map
  input:
    over: ["one", "two", "three", "four"]
    concurrency: 4
    do:
    - id: file
      tool_name: t__evaluate
      files:
        snapshot: { name: snapshot.json }
      input:
        content: "{{item}}"
        filename: "{{files.snapshot.path}}"
"#,
    );
    let mut output = Map::new();
    output.insert("rows".into(), json!("{{saved.results}}"));
    let outcome = pipeline(workspace.path())
        .run_explicit("q", plan, Finish::Output(output), None)
        .await
        .unwrap();
    let rows = outcome.structured.unwrap()["rows"]
        .as_array()
        .unwrap()
        .clone();
    let mut refs: Vec<String> = rows
        .iter()
        .map(|row| row["files"][0]["ref"].as_str().unwrap().to_string())
        .collect();
    refs.sort();
    refs.dedup();
    assert_eq!(refs.len(), 4, "{rows:?}");
}

#[tokio::test]
async fn a_glob_collects_every_page_as_a_list() {
    let workspace = tempfile::tempdir().unwrap();
    let plan = steps(
        r#"
- id: pages
  tool_name: t__render
  files:
    pages: { name: "page-*.png" }
  input:
    content: p
    dir: "{{files.pages.dir}}"
"#,
    );
    let mut output = Map::new();
    output.insert("count".into(), json!("{{pages.files.pages.length}}"));
    output.insert("second".into(), json!("{{pages.files.pages.1.bytes}}"));
    let outcome = pipeline(workspace.path())
        .run_explicit("q", plan, Finish::Output(output), None)
        .await
        .unwrap();
    assert_eq!(outcome.structured, Some(json!({"count": 3, "second": 2})));
}

#[tokio::test]
async fn a_file_the_tool_never_wrote_fails_the_step() {
    let workspace = tempfile::tempdir().unwrap();
    let plan = steps(
        r#"
- id: shot
  tool_name: t__evaluate
  files:
    snapshot: { name: snapshot.json }
  input:
    content: x
"#,
    );
    let error = pipeline(workspace.path())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("didn't write the declared file \"snapshot\" (snapshot.json)"),
        "{error}"
    );
}

#[tokio::test]
async fn a_step_with_files_and_no_file_space_is_refused() {
    let plan = steps(
        r#"
- id: shot
  tool_name: t__evaluate
  files:
    snapshot: { name: snapshot.json }
  input:
    filename: "{{files.snapshot.path}}"
"#,
    );
    let pipeline = super::tests::pipeline_for(Arc::new(Writer));
    let error = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no file storage"), "{error}");
}

async fn problems(steps: &str, output: &str) -> Vec<String> {
    let yaml = format!(
        "version: 3\nidentifier: p\nname: P\ndescription: d\nsteps:\n{steps}\nfinish:\n  output:\n{output}\n"
    );
    let doc = super::doc::parse_plan_source(&yaml, "plan").unwrap();
    super::tests::pipeline_for(Arc::new(Writer))
        .plan_problems(&doc)
        .await
}

#[tokio::test]
async fn a_well_formed_declaration_has_no_named_file_problems() {
    let found = problems(
        r#"- id: shot
  tool_name: t__evaluate
  files:
    snapshot: { name: snapshot.json }
    pages: { name: "page-*.png" }
  input:
    filename: "{{files.snapshot.path}}"
    dir: "{{files.pages.dir}}""#,
        r#"    ref: "{{shot.files.snapshot.ref}}"
    first: "{{shot.files.pages.0.ref}}"
    all: "{{shot.files.pages}}""#,
    )
    .await;
    assert!(
        !found.iter().any(|problem| problem.contains("file")),
        "{found:?}"
    );
}

#[tokio::test]
async fn each_misuse_of_a_declaration_is_reported() {
    let found = problems(
        r#"- id: shot
  tool_name: t__evaluate
  files:
    snapshot: { name: snapshot.json }
    unused: { name: unused.txt }
    pages: { name: "*.png" }
    cover: { name: "cover.png" }
  input:
    filename: "{{files.snapshot.path}}"
    dir: "{{files.pages.path}}"
    other: "{{files.nope.path}}"
    cover: "{{files.cover.path}}""#,
        r#"    missing: "{{shot.files.nothing.ref}}"
    glob: "{{shot.files.pages.ref}}""#,
    )
    .await;
    let expect = [
        "declares the file `unused` but never passes {{files.unused.path}}",
        "`pages` is a pattern (*.png), so it has no single path",
        "`{{files.nope.path}}` names no declared file",
        "files `cover` (cover.png) and `pages` (*.png) can match the same file",
        "step shot declares no file `nothing`",
        "`pages` is a pattern (*.png), so it's a list; pick one with shot.files.pages.0.ref",
    ];
    for expected in expect {
        assert!(
            found.iter().any(|problem| problem.contains(expected)),
            "missing {expected:?} in {found:?}"
        );
    }
}
