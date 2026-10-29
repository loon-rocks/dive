use super::*;
use crate::files::memory::Memory;
use crate::files::FileDecl;

fn decls(pairs: &[(&str, &str, bool)]) -> FileDecls {
    pairs
        .iter()
        .map(|(key, name, optional)| {
            (
                key.to_string(),
                FileDecl {
                    name: name.to_string(),
                    optional: *optional,
                },
            )
        })
        .collect()
}

fn space(workspace: &Path) -> FileSpace {
    FileSpace {
        store: Arc::new(Memory::default()),
        dir: workspace.join(".dive/files"),
    }
}

#[tokio::test]
async fn a_declared_file_comes_back_by_name_and_its_directory_is_removed() {
    let workspace = tempfile::tempdir().unwrap();
    let named = space(workspace.path())
        .prepare(&decls(&[("snapshot", "snapshot.json", false)]))
        .unwrap();
    let root = named.root();
    let path = root["snapshot"]["path"].as_str().unwrap().to_string();
    assert!(path.starts_with(
        &workspace
            .path()
            .join(".dive/files/tmp")
            .display()
            .to_string()
    ));
    std::fs::write(&path, "{\"a\": 1}").unwrap();
    let result = named.collect(json!({"text": "saved"})).await.unwrap();
    assert_eq!(result["text"], "saved");
    assert_eq!(result["files"][0]["name"], "snapshot");
    assert_eq!(result["files"][0]["mime"], "application/json");
    assert!(result["files"][0].get("many").is_none());
    let dir = named.dir().to_path_buf();
    drop(named);
    assert!(!dir.exists());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(".dive/files/.gitignore")).unwrap(),
        "*\n"
    );
}

#[tokio::test]
async fn a_glob_collects_matching_files_in_natural_order_and_skips_the_rest() {
    let workspace = tempfile::tempdir().unwrap();
    let named = space(workspace.path())
        .prepare(&decls(&[("pages", "page-*.png", false)]))
        .unwrap();
    assert!(named.root()["pages"].get("path").is_none());
    for name in ["page-10.png", "page-9.png", "page-1.png", "cover.png"] {
        std::fs::write(named.dir().join(name), name).unwrap();
    }
    let result = named.collect(json!({})).await.unwrap();
    let files = result["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    assert!(files
        .iter()
        .all(|file| file["many"] == true && file["name"] == "pages"));
    let sizes: Vec<u64> = files
        .iter()
        .map(|file| file["bytes"].as_u64().unwrap())
        .collect();
    assert_eq!(sizes, [10, 10, 11]);
}

#[tokio::test]
async fn a_missing_file_fails_unless_it_is_optional() {
    let workspace = tempfile::tempdir().unwrap();
    let space = space(workspace.path());
    let required = space
        .prepare(&decls(&[("snapshot", "snapshot.json", false)]))
        .unwrap();
    let error = required.collect(json!({})).await.unwrap_err();
    assert_eq!(
        error,
        "the tool didn't write the declared file \"snapshot\" (snapshot.json)"
    );
    let optional = space
        .prepare(&decls(&[("trace", "trace-*.zip", true)]))
        .unwrap();
    assert_eq!(
        optional.collect(json!({"a": 1})).await.unwrap(),
        json!({"a": 1})
    );
}

#[tokio::test]
async fn a_file_the_tool_also_returned_is_named_in_place() {
    let workspace = tempfile::tempdir().unwrap();
    let named = space(workspace.path())
        .prepare(&decls(&[("shot", "shot.png", false)]))
        .unwrap();
    std::fs::write(named.dir().join("shot.png"), "pixels").unwrap();
    let returned = named.store.put_as(b"pixels".to_vec(), "png").await.unwrap();
    let result = json!({"files": [serde_json::to_value(&returned).unwrap()]});
    let merged = named.collect(result).await.unwrap();
    let files = merged["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["name"], "shot");
}

#[tokio::test]
async fn named_and_positional_paths_read_the_same_entry_and_a_glob_reads_as_a_list() {
    let workspace = tempfile::tempdir().unwrap();
    let named = space(workspace.path())
        .prepare(&decls(&[
            ("snapshot", "snapshot.json", false),
            ("pages", "page-*.png", false),
        ]))
        .unwrap();
    std::fs::write(named.dir().join("snapshot.json"), "{}").unwrap();
    std::fs::write(named.dir().join("page-1.png"), "p1").unwrap();
    let result = named.collect(json!({})).await.unwrap();
    let mut roots = Map::new();
    roots.insert("shot".to_string(), result);
    let roots = crate::template::Roots::new(&roots);
    let render = |template: &str| crate::template::render_value(template, &roots).unwrap();
    let by_name = render("{{shot.files.snapshot.ref}}");
    assert!(by_name.as_str().unwrap().ends_with(".json"));
    let pages = render("{{shot.files.pages}}");
    assert_eq!(pages.as_array().unwrap().len(), 1);
    assert!(render("{{shot.files.pages.0.ref}}")
        .as_str()
        .unwrap()
        .ends_with(".png"));
    let missing = crate::template::render_value("{{shot.files.nope.ref}}", &roots).unwrap_err();
    assert!(missing.to_string().contains("named: "), "{missing}");
}
