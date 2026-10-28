use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

pub const BLOB_PREFIX: &str = "blob:sha256-";

const TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("tiff", "image/tiff"),
    ("svg", "image/svg+xml"),
    ("wav", "audio/wav"),
    ("mp3", "audio/mpeg"),
    ("ogg", "audio/ogg"),
    ("flac", "audio/flac"),
    ("m4a", "audio/mp4"),
    ("pdf", "application/pdf"),
    ("json", "application/json"),
    ("txt", "text/plain"),
    ("bin", "application/octet-stream"),
];

pub fn extension_for(mime: &str) -> &'static str {
    let mime = mime.split(';').next().unwrap_or_default().trim();
    let mime = match mime {
        "image/jpg" => "image/jpeg",
        "audio/x-wav" | "audio/wave" => "audio/wav",
        "audio/mp3" => "audio/mpeg",
        other => other,
    };
    TYPES
        .iter()
        .find(|(_, known)| known.eq_ignore_ascii_case(mime))
        .map_or("bin", |(extension, _)| extension)
}

pub fn mime_for(extension: &str) -> &'static str {
    let extension = extension.to_ascii_lowercase();
    let extension = match extension.as_str() {
        "jpeg" => "jpg",
        "tif" => "tiff",
        other => other,
    };
    TYPES
        .iter()
        .find(|(known, _)| *known == extension)
        .map_or("application/octet-stream", |(_, mime)| mime)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlobRef {
    hash: String,
    extension: String,
}

impl BlobRef {
    pub fn new(hash: impl Into<String>, mime: &str) -> Self {
        Self {
            hash: hash.into(),
            extension: extension_for(mime).to_string(),
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(BLOB_PREFIX)?;
        let (hash, extension) = rest.split_once('.')?;
        let hex = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        let known = TYPES.iter().any(|(known, _)| *known == extension);
        (hex && known).then(|| Self {
            hash: hash.to_ascii_lowercase(),
            extension: extension.to_string(),
        })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn extension(&self) -> &str {
        &self.extension
    }

    pub fn mime(&self) -> &'static str {
        mime_for(&self.extension)
    }

    pub fn file_name(&self) -> String {
        format!("{}.{}", self.hash, self.extension)
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{BLOB_PREFIX}{}.{}", self.hash, self.extension)
    }
}

impl TryFrom<String> for BlobRef {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or_else(|| format!("not a file reference: {text}"))
    }
}

impl From<BlobRef> for String {
    fn from(blob: BlobRef) -> Self {
        blob.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFile {
    #[serde(rename = "ref")]
    pub blob: BlobRef,
    pub mime: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileLocation {
    Path(PathBuf),
    Url(String),
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct FileError(pub String);

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, bytes: Vec<u8>, mime: &str) -> Result<StoredFile, FileError>;

    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, FileError>;

    async fn delete(&self, blob: &BlobRef) -> Result<(), FileError>;

    async fn locate(&self, blob: &BlobRef) -> Result<FileLocation, FileError>;

    async fn materialize(&self, blob: &BlobRef) -> Result<PathBuf, FileError>;

    fn shared(&self) -> bool {
        false
    }
}

pub const MODEL_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

pub fn model_image_refs_in(text: &str) -> Vec<BlobRef> {
    refs_in_text(text)
        .into_iter()
        .filter(|blob| MODEL_IMAGE_TYPES.contains(&blob.mime()))
        .collect()
}

pub struct FileImages(pub std::sync::Arc<dyn BlobStore>);

#[async_trait]
impl dive_llm::ImageSource for FileImages {
    fn refs_in(&self, text: &str) -> Vec<String> {
        model_image_refs_in(text)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    async fn load(&self, reference: &str) -> Result<dive_llm::ImageData, String> {
        let blob = BlobRef::parse(reference)
            .ok_or_else(|| format!("not a file reference: {reference}"))?;
        let bytes = self.0.get(&blob).await.map_err(|e| e.to_string())?;
        Ok(dive_llm::ImageData {
            mime: blob.mime().to_string(),
            bytes: bytes.into(),
        })
    }
}

pub fn attach(result: Value, stored: Vec<StoredFile>) -> Value {
    if stored.is_empty() {
        return result;
    }
    let listed = serde_json::to_value(stored).unwrap_or_else(|_| Value::Array(Vec::new()));
    match result {
        Value::Object(mut map) if !map.contains_key("files") => {
            map.insert("files".to_string(), listed);
            Value::Object(map)
        }
        other => serde_json::json!({ "result": other, "files": listed }),
    }
}

pub fn refs_in_text(text: &str) -> Vec<BlobRef> {
    let mut found = Vec::new();
    for (at, _) in text.match_indices(BLOB_PREFIX) {
        let rest = &text[at..];
        let end = rest[BLOB_PREFIX.len()..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
            .map_or(rest.len(), |end| end + BLOB_PREFIX.len());
        let candidate = rest[..end].trim_end_matches('.');
        if let Some(blob) = BlobRef::parse(candidate) {
            if !found.contains(&blob) {
                found.push(blob);
            }
        }
    }
    found
}

pub fn refs_in(value: &Value) -> Vec<BlobRef> {
    let mut found = Vec::new();
    collect_refs(value, &mut found);
    found.dedup();
    found
}

fn collect_refs(value: &Value, found: &mut Vec<BlobRef>) {
    match value {
        Value::String(text) => {
            if let Some(blob) = BlobRef::parse(text) {
                if !found.contains(&blob) {
                    found.push(blob);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_refs(item, found)),
        Value::Object(map) => map.values().for_each(|item| collect_refs(item, found)),
        _ => {}
    }
}

pub fn without_refs(value: Value) -> Value {
    match value {
        Value::String(text) => {
            let mut text = text;
            for blob in refs_in_text(&text) {
                text = text.replace(
                    &blob.to_string(),
                    &format!("{BLOB_PREFIX}<hash>.{}", blob.extension()),
                );
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(without_refs).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, without_refs(item)))
                .collect(),
        ),
        other => other,
    }
}

pub fn substitute_refs(value: Value, replacements: &HashMap<BlobRef, Value>) -> Value {
    match value {
        Value::String(text) => match BlobRef::parse(&text) {
            Some(blob) => replacements
                .get(&blob)
                .cloned()
                .unwrap_or(Value::String(text)),
            None => Value::String(text),
        },
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| substitute_refs(item, replacements))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, substitute_refs(item, replacements)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_ref_is_replaced_with_a_placeholder_that_names_no_file() {
        let blob = BlobRef::new("ab".repeat(32), "image/png").to_string();
        let example = json!({"files": [{"ref": blob}], "text": format!("saved {blob}.")});
        let cleaned = without_refs(example);
        assert_eq!(cleaned["files"][0]["ref"], "blob:sha256-<hash>.png");
        assert_eq!(cleaned["text"], "saved blob:sha256-<hash>.png.");
        assert!(refs_in(&cleaned).is_empty());
        assert!(refs_in_text(&cleaned.to_string()).is_empty());
    }

    use super::*;
    use serde_json::json;

    const HASH: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[test]
    fn a_ref_round_trips_through_its_text_and_json() {
        let blob = BlobRef::new(HASH, "image/png");
        let text = blob.to_string();
        assert_eq!(text, format!("blob:sha256-{HASH}.png"));
        assert_eq!(BlobRef::parse(&text), Some(blob.clone()));
        assert_eq!(serde_json::to_value(&blob).unwrap(), json!(text));
        assert_eq!(
            serde_json::from_value::<BlobRef>(json!(text)).unwrap(),
            blob
        );
        assert_eq!(blob.mime(), "image/png");
        assert_eq!(blob.file_name(), format!("{HASH}.png"));
    }

    #[test]
    fn text_that_only_looks_like_a_ref_is_not_one() {
        for text in [
            "blob:sha256-abc.png",
            &format!("blob:sha256-{HASH}"),
            &format!("blob:sha256-{HASH}.exe"),
            &format!("see blob:sha256-{HASH}.png"),
            &format!("blob:sha256-{}.png", "z".repeat(64)),
        ] {
            assert_eq!(BlobRef::parse(text), None, "{text}");
        }
    }

    #[test]
    fn mime_types_map_to_extensions_and_back() {
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("image/jpg"), "jpg");
        assert_eq!(extension_for("IMAGE/PNG"), "png");
        assert_eq!(extension_for("audio/wav; codecs=1"), "wav");
        assert_eq!(extension_for("application/x-unknown"), "bin");
        assert_eq!(mime_for("jpg"), "image/jpeg");
        assert_eq!(mime_for("nope"), "application/octet-stream");
    }

    #[test]
    fn refs_are_found_and_replaced_at_any_depth_only_as_whole_strings() {
        let blob = BlobRef::new(HASH, "image/png");
        let input = json!({
            "image": blob.to_string(),
            "more": [{"again": blob.to_string()}, "plain"],
            "prose": format!("look at {blob}"),
            "count": 2,
        });
        assert_eq!(refs_in(&input), vec![blob.clone()]);
        let text = format!("See ![one]({blob}). Also {blob}, and blob:sha256-nothex.png");
        assert_eq!(refs_in_text(&text), vec![blob.clone()]);
        let replacements = HashMap::from([(blob.clone(), json!("/tmp/a.png"))]);
        assert_eq!(
            substitute_refs(input, &replacements),
            json!({
                "image": "/tmp/a.png",
                "more": [{"again": "/tmp/a.png"}, "plain"],
                "prose": format!("look at {blob}"),
                "count": 2,
            })
        );
    }
}
