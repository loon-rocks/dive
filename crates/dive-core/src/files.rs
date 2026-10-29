use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

#[cfg(test)]
pub(crate) mod memory;
pub mod named;

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
    ("html", "text/html"),
    ("css", "text/css"),
    ("csv", "text/csv"),
    ("md", "text/markdown"),
    ("yaml", "application/yaml"),
    ("xml", "application/xml"),
    ("js", "text/javascript"),
    ("zip", "application/zip"),
    ("mp4", "video/mp4"),
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
        .map(|(extension, _)| *extension)
        .or_else(|| {
            mime_guess::get_mime_extensions_str(&mime.to_ascii_lowercase())
                .and_then(|extensions| extensions.first().copied())
        })
        .unwrap_or("bin")
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
        .map(|(_, mime)| *mime)
        .or_else(|| mime_guess::from_ext(extension).first_raw())
        .unwrap_or("application/octet-stream")
}

pub fn valid_extension(extension: &str) -> bool {
    (1..=16).contains(&extension.len())
        && extension
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlobRef {
    hash: String,
    extension: String,
}

impl BlobRef {
    pub fn new(hash: impl Into<String>, mime: &str) -> Self {
        Self::with_extension(hash, extension_for(mime))
    }

    pub fn with_extension(hash: impl Into<String>, extension: &str) -> Self {
        let extension = extension.to_ascii_lowercase();
        Self {
            hash: hash.into(),
            extension: if valid_extension(&extension) {
                extension
            } else {
                "bin".to_string()
            },
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(BLOB_PREFIX)?;
        let (hash, extension) = rest.split_once('.')?;
        let hex = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (hex && valid_extension(extension)).then(|| Self {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    async fn put(&self, bytes: Vec<u8>, mime: &str) -> Result<StoredFile, FileError> {
        self.put_as(bytes, extension_for(mime)).await
    }

    async fn put_as(&self, bytes: Vec<u8>, extension: &str) -> Result<StoredFile, FileError>;

    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, FileError>;

    async fn delete(&self, blob: &BlobRef) -> Result<(), FileError>;

    async fn locate(&self, blob: &BlobRef) -> Result<FileLocation, FileError>;

    async fn materialize(&self, blob: &BlobRef) -> Result<PathBuf, FileError>;

    fn shared(&self) -> bool {
        false
    }
}

pub const MODEL_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

pub const PDF_TYPE: &str = "application/pdf";

const TEXT_TYPES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/yaml",
    "application/x-yaml",
    "application/toml",
    "application/javascript",
    "application/x-javascript",
    "application/x-sh",
    "application/sql",
    "image/svg+xml",
];

pub fn is_text(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || TEXT_TYPES.contains(&mime)
}

pub fn model_image_refs_in(text: &str) -> Vec<BlobRef> {
    refs_in_text(text)
        .into_iter()
        .filter(|blob| MODEL_IMAGE_TYPES.contains(&blob.mime()) || blob.mime() == PDF_TYPE)
        .collect()
}

pub fn text_refs_in(text: &str) -> Vec<BlobRef> {
    refs_in_text(text)
        .into_iter()
        .filter(|blob| is_text(blob.mime()))
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

    fn text_refs_in(&self, text: &str) -> Vec<String> {
        text_refs_in(text).iter().map(ToString::to_string).collect()
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
    attach_entries(
        result,
        stored
            .iter()
            .filter_map(|file| serde_json::to_value(file).ok())
            .collect(),
    )
}

pub fn attach_entries(result: Value, stored: Vec<Value>) -> Value {
    if stored.is_empty() {
        return result;
    }
    let listed = Value::Array(stored);
    match result {
        Value::Object(mut map) if !map.contains_key("files") => {
            map.insert("files".to_string(), listed);
            Value::Object(map)
        }
        other => serde_json::json!({ "result": other, "files": listed }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileDecl {
    #[schemars(
        description = "The file's name with its extension, such as snapshot.json, or a pattern with * or ? such as page-*.png for several files"
    )]
    pub name: String,
    #[schemars(
        description = "When true, a missing file, or a pattern with no match, isn't an error"
    )]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

pub type FileDecls = std::collections::BTreeMap<String, FileDecl>;

impl FileDecl {
    pub fn is_glob(&self) -> bool {
        self.name.contains(['*', '?'])
    }

    pub fn matches(&self, file_name: &str) -> bool {
        glob_match(&self.name, file_name)
    }
}

pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let chunks = |text: &str| -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in text.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((last, chunk)) if *last == digit => chunk.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    };
    let (a, b) = (chunks(a), chunks(b));
    for ((a_digit, a), (b_digit, b)) in a.iter().zip(&b) {
        let order = if *a_digit && *b_digit {
            let (a_trim, b_trim) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
            a_trim
                .len()
                .cmp(&b_trim.len())
                .then_with(|| a_trim.cmp(b_trim))
        } else {
            a.cmp(b)
        };
        if order.is_ne() {
            return order;
        }
    }
    a.len().cmp(&b.len())
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
            &format!("blob:sha256-{HASH}.PNG"),
            &format!("blob:sha256-{HASH}.{}", "a".repeat(17)),
            &format!("blob:sha256-{HASH}.tar/gz"),
            &format!("see blob:sha256-{HASH}.png"),
            &format!("blob:sha256-{}.png", "z".repeat(64)),
        ] {
            assert_eq!(BlobRef::parse(text), None, "{text}");
        }
    }

    #[test]
    fn any_short_lowercase_extension_is_a_ref_and_keeps_its_type() {
        for (extension, mime) in [
            ("css", "text/css"),
            ("zip", "application/zip"),
            ("parquet", "application/octet-stream"),
            ("exe", "application/octet-stream"),
        ] {
            let text = format!("blob:sha256-{HASH}.{extension}");
            let blob = BlobRef::parse(&text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(blob.extension(), extension);
            assert_ne!(blob.mime(), "", "{text}");
            if mime != "application/octet-stream" {
                assert_eq!(blob.mime(), mime);
            }
        }
        assert_eq!(BlobRef::with_extension(HASH, "CSS").extension(), "css");
        assert_eq!(BlobRef::with_extension(HASH, "../x").extension(), "bin");
    }

    #[test]
    fn mime_types_map_to_extensions_and_back() {
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("image/jpg"), "jpg");
        assert_eq!(extension_for("IMAGE/PNG"), "png");
        assert_eq!(extension_for("audio/wav; codecs=1"), "wav");
        assert_eq!(extension_for("application/x-unknown"), "bin");
        assert_eq!(extension_for("text/html"), "html");
        assert_eq!(mime_for("css"), "text/css");
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
