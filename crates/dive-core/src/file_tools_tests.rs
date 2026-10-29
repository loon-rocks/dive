use super::*;
use crate::files::{FileError, FileLocation, StoredFile};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Default)]
struct Memory(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait]
impl BlobStore for Memory {
    async fn put_as(&self, bytes: Vec<u8>, extension: &str) -> Result<StoredFile, FileError> {
        let hash = format!("{:064x}", bytes.iter().map(|b| *b as u64).sum::<u64>());
        let blob = BlobRef::with_extension(hash, extension);
        let size = bytes.len() as u64;
        self.0.lock().unwrap().insert(blob.to_string(), bytes);
        Ok(StoredFile {
            name: None,
            mime: blob.mime().to_string(),
            blob,
            bytes: size,
            width: None,
            height: None,
        })
    }

    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, FileError> {
        self.0
            .lock()
            .unwrap()
            .get(&blob.to_string())
            .cloned()
            .ok_or_else(|| FileError(format!("no {blob}")))
    }

    async fn delete(&self, _blob: &BlobRef) -> Result<(), FileError> {
        Ok(())
    }

    async fn locate(&self, blob: &BlobRef) -> Result<FileLocation, FileError> {
        Ok(FileLocation::Path(PathBuf::from(blob.file_name())))
    }

    async fn materialize(&self, blob: &BlobRef) -> Result<PathBuf, FileError> {
        Ok(PathBuf::from(blob.file_name()))
    }
}

fn tools() -> FileTools {
    FileTools::new(Arc::new(Memory::default()))
}

async fn call(tools: &FileTools, name: &str, input: Value) -> ToolOutcome {
    tools.invoke(name, input).await.unwrap()
}

#[tokio::test]
async fn an_object_is_saved_as_json_and_reads_back_as_the_same_value() {
    let tools = tools();
    let data = json!({"customCss": ":root { --accent: #2563EB; }", "heading": "What's new"});
    let saved = call(
        &tools,
        SAVE_FILE_TOOL,
        json!({"name": "pro_1-custom_css.json", "content": data}),
    )
    .await;
    assert!(!saved.is_error, "{:?}", saved.result);
    assert_eq!(saved.result["name"], "pro_1-custom_css.json");
    assert_eq!(saved.result["mime"], "application/json");
    assert!(saved.result["ref"].as_str().unwrap().ends_with(".json"));
    assert_eq!(saved.result["files"][0]["ref"], saved.result["ref"]);

    let read = call(&tools, READ_FILE_TOOL, json!({"ref": saved.result["ref"]})).await;
    assert!(!read.is_error, "{:?}", read.result);
    assert_eq!(read.result["json"], data);
}

#[tokio::test]
async fn a_string_is_saved_byte_for_byte_with_the_type_its_extension_names() {
    let tools = tools();
    let css = "body { color: red; }\n\"quoted\" é";
    let saved = call(
        &tools,
        SAVE_FILE_TOOL,
        json!({"name": "styles.css", "content": css}),
    )
    .await;
    assert_eq!(saved.result["mime"], "text/css");
    let read = call(&tools, READ_FILE_TOOL, json!({"ref": saved.result["ref"]})).await;
    assert_eq!(read.result["text"], css);
    assert!(read.result.get("json").is_none());
}

#[tokio::test]
async fn an_unknown_extension_is_kept() {
    let tools = tools();
    let saved = call(
        &tools,
        SAVE_FILE_TOOL,
        json!({"name": "notes.dive", "content": "x"}),
    )
    .await;
    assert!(saved.result["ref"].as_str().unwrap().ends_with(".dive"));
    assert_eq!(saved.result["mime"], "application/octet-stream");
}

#[tokio::test]
async fn a_name_without_an_extension_takes_one_from_the_content() {
    let tools = tools();
    let text = call(
        &tools,
        SAVE_FILE_TOOL,
        json!({"name": "notes", "content": "x"}),
    )
    .await;
    assert!(text.result["ref"].as_str().unwrap().ends_with(".txt"));
    let typed = call(
        &tools,
        SAVE_FILE_TOOL,
        json!({"name": "page", "content": "<p>x</p>", "type": "text/html"}),
    )
    .await;
    assert!(typed.result["ref"].as_str().unwrap().ends_with(".html"));
}

#[tokio::test]
async fn a_path_for_a_name_is_refused() {
    let saved = call(
        &tools(),
        SAVE_FILE_TOOL,
        json!({"name": "../snap.json", "content": "x"}),
    )
    .await;
    assert!(saved.is_error);
    assert!(saved.result["error"]
        .as_str()
        .unwrap()
        .contains("not a path"));
}

#[tokio::test]
async fn a_binary_file_is_refused_with_what_to_do_instead() {
    let tools = tools();
    let saved = call(
        &tools,
        SAVE_FILE_TOOL,
        json!({"name": "shot.png", "content": "not really a png"}),
    )
    .await;
    let read = call(&tools, READ_FILE_TOOL, json!({"ref": saved.result["ref"]})).await;
    assert!(read.is_error);
    assert!(read.result["error"]
        .as_str()
        .unwrap()
        .contains("pass the ref to a model or a tool"));
}

#[tokio::test]
async fn a_long_file_is_cut_at_max_bytes_on_a_character_boundary() {
    let tools = tools();
    let saved = call(
        &tools,
        SAVE_FILE_TOOL,
        json!({"name": "long.txt", "content": "ééééé"}),
    )
    .await;
    let read = call(
        &tools,
        READ_FILE_TOOL,
        json!({"ref": saved.result["ref"], "max_bytes": 5}),
    )
    .await;
    assert_eq!(read.result["text"], "éé");
    assert_eq!(read.result["truncated"], true);
}

#[tokio::test]
async fn something_that_is_not_a_ref_is_refused() {
    let read = call(
        &tools(),
        READ_FILE_TOOL,
        json!({"ref": ".dive/snapshots/a.json"}),
    )
    .await;
    assert!(read.is_error);
    assert!(read.result["error"].as_str().unwrap().contains("file ref"));
}
