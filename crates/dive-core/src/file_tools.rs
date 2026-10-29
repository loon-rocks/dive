use crate::files::{attach, extension_for, is_text, valid_extension, BlobRef, BlobStore};
use crate::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub const SAVE_FILE_TOOL: &str = "builtin__save_file";
pub const READ_FILE_TOOL: &str = "builtin__read_file";

pub const FILE_TOOLS: [&str; 2] = [SAVE_FILE_TOOL, READ_FILE_TOOL];

const DEFAULT_READ_BYTES: usize = 1024 * 1024;

pub struct FileTools {
    files: Arc<dyn BlobStore>,
}

impl FileTools {
    pub fn new(files: Arc<dyn BlobStore>) -> Self {
        Self { files }
    }

    async fn save(&self, input: Value) -> Result<Value, String> {
        let name = input["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or(
                "save_file needs a `name`, a file name with its extension such as snapshot.json",
            )?;
        if name.contains(['/', '\\']) {
            return Err(format!(
                "`name` is a file name, not a path: \"{name}\" has a slash"
            ));
        }
        let content = &input["content"];
        let bytes = match content {
            Value::Null => return Err("save_file needs `content`".to_string()),
            Value::String(text) => text.clone().into_bytes(),
            other => {
                let mut text = serde_json::to_string_pretty(other).map_err(|e| e.to_string())?;
                text.push('\n');
                text.into_bytes()
            }
        };
        let extension = match name.rsplit_once('.') {
            Some((_, extension)) if valid_extension(&extension.to_ascii_lowercase()) => {
                extension.to_ascii_lowercase()
            }
            _ => match input["type"].as_str() {
                Some(mime) => extension_for(mime).to_string(),
                None if content.is_string() => "txt".to_string(),
                None => "json".to_string(),
            },
        };
        let mut stored = self
            .files
            .put_as(bytes, &extension)
            .await
            .map_err(|e| e.to_string())?;
        stored.name = Some(name.to_string());
        let saved = serde_json::to_value(&stored).map_err(|e| e.to_string())?;
        Ok(attach(saved, vec![stored]))
    }

    async fn read(&self, input: Value) -> Result<Value, String> {
        let text = input["ref"].as_str().unwrap_or_default();
        let blob = BlobRef::parse(text).ok_or_else(|| {
            format!("`ref` must be a file ref like blob:sha256-….json, got \"{text}\"")
        })?;
        let mime = blob.mime();
        if !is_text(mime) {
            return Err(format!(
                "{blob} is {mime}, not text; pass the ref to a model or a tool instead"
            ));
        }
        let max = input["max_bytes"]
            .as_u64()
            .map_or(DEFAULT_READ_BYTES, |max| max as usize);
        let bytes = self.files.get(&blob).await.map_err(|e| e.to_string())?;
        let whole = String::from_utf8_lossy(&bytes);
        let text = dive_llm::image_input::cut_at(&whole, max);
        let mut read = json!({
            "ref": blob.to_string(),
            "mime": mime,
            "bytes": bytes.len(),
            "text": text,
        });
        if text.len() < whole.len() {
            read["truncated"] = json!(true);
        } else if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            read["json"] = parsed;
        }
        Ok(read)
    }
}

#[async_trait]
impl ToolRegistry for FileTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(file_tool_defs())
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let result = match name {
            SAVE_FILE_TOOL => self.save(input).await,
            READ_FILE_TOOL => self.read(input).await,
            _ => return Err(ToolError::Unknown(name.to_string())),
        };
        Ok(match result {
            Ok(result) => ToolOutcome {
                result,
                is_error: false,
            },
            Err(error) => ToolOutcome {
                result: json!({ "error": error }),
                is_error: true,
            },
        })
    }
}

pub fn file_tool_defs() -> Vec<ToolDef> {
    let file = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "ref": {"type": "string", "format": "blob"},
            "mime": {"type": "string"},
            "bytes": {"type": "integer"}
        }
    });
    vec![
        ToolDef {
            name: SAVE_FILE_TOOL.to_string(),
            description: "Store text or JSON you already have as a file and get its ref, in code \
                with no model. `content` is a string, written as it is, or an object or list, \
                written as pretty JSON. `name` is the file name with its extension, such as \
                snapshot.json or styles.css, and the extension sets the file's type. Returns \
                {name, ref, mime, bytes}; pass the ref on, and read it back with \
                builtin__read_file."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["name", "content"],
                "properties": {
                    "name": {"type": "string", "description": "File name with extension, such as snapshot.json"},
                    "content": {"description": "The text, or an object or list to store as JSON"},
                    "type": {"type": "string", "description": "MIME type, used only when the name has no extension"}
                }
            }),
            output_schema: Some(file.clone()),
            output_example: None,
            read_only: Some(true),
            file_input: Default::default(),
        },
        ToolDef {
            name: READ_FILE_TOOL.to_string(),
            description: "Read a text file's content from its ref, in code with no model: JSON, \
                CSS, HTML, Markdown, CSV, YAML and other text types. Returns {ref, mime, bytes, \
                text}, plus `json` when the text parses as JSON, and `truncated` when the file \
                is longer than `max_bytes` (default 1 MB). Images, PDFs and other binary files \
                are refused; pass their refs to a model or a tool instead."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["ref"],
                "properties": {
                    "ref": {"type": "string", "format": "blob", "description": "A file ref such as blob:sha256-….json"},
                    "max_bytes": {"type": "integer", "description": "Most bytes to return; default 1048576"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "format": "blob"},
                    "mime": {"type": "string"},
                    "bytes": {"type": "integer"},
                    "text": {"type": "string"},
                    "json": {},
                    "truncated": {"type": "boolean"}
                }
            })),
            output_example: None,
            read_only: Some(true),
            file_input: Default::default(),
        },
    ]
}

#[cfg(test)]
#[path = "file_tools_tests.rs"]
mod tests;
