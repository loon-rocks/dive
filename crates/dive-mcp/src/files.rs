use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use dive_config::FilesMode;
use dive_core::files::{refs_in, substitute_refs, BlobStore, FileLocation, StoredFile};
use rmcp::model::{ContentBlock, ResourceContents};
use serde_json::{json, Value};
use std::collections::HashMap;

pub(crate) struct Binary {
    pub data: String,
    pub mime: String,
}

pub(crate) struct Link {
    pub uri: String,
}

pub(crate) const MAX_LINKED_BYTES: usize = 50 * 1024 * 1024;

pub(crate) fn binaries(content: &[ContentBlock]) -> (Vec<Binary>, Vec<Link>) {
    let mut found = Vec::new();
    let mut links = Vec::new();
    for block in content {
        match block {
            ContentBlock::Image(image) => found.push(Binary {
                data: image.data.clone(),
                mime: image.mime_type.clone(),
            }),
            ContentBlock::Audio(audio) => found.push(Binary {
                data: audio.data.clone(),
                mime: audio.mime_type.clone(),
            }),
            ContentBlock::Resource(embedded) => found.extend(resource_file(&embedded.resource)),
            ContentBlock::ResourceLink(link) => links.push(Link {
                uri: link.uri.clone(),
            }),
            _ => {}
        }
    }
    (found, links)
}

pub(crate) fn resource_file(contents: &ResourceContents) -> Option<Binary> {
    Some(match contents {
        ResourceContents::BlobResourceContents {
            blob, mime_type, ..
        } => Binary {
            data: blob.clone(),
            mime: mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        },
        ResourceContents::TextResourceContents {
            text, mime_type, ..
        } => Binary {
            data: STANDARD.encode(text),
            mime: mime_type
                .clone()
                .unwrap_or_else(|| "text/plain".to_string()),
        },
        _ => return None,
    })
}

impl Binary {
    pub(crate) fn decoded_len(&self) -> usize {
        self.data.trim().len() / 4 * 3
    }
}

pub(crate) async fn store(files: &dyn BlobStore, found: Vec<Binary>) -> Vec<StoredFile> {
    let mut stored = Vec::new();
    for binary in found {
        let bytes = match STANDARD.decode(binary.data.trim()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(mime = binary.mime, error = %e, "skipping a file that isn't valid base64");
                continue;
            }
        };
        match files.put(bytes, &binary.mime).await {
            Ok(file) => stored.push(file),
            Err(e) => {
                tracing::warn!(mime = binary.mime, error = %e, "skipping a file that couldn't be stored")
            }
        }
    }
    stored
}

pub(crate) fn attach(result: Value, stored: Vec<StoredFile>) -> Value {
    dive_core::files::attach(result, stored)
}

pub(crate) async fn pass(
    input: Value,
    files: &dyn BlobStore,
    mode: FilesMode,
) -> Result<Value, String> {
    if mode == FilesMode::Off {
        return Ok(input);
    }
    let refs = refs_in(&input);
    if refs.is_empty() {
        return Ok(input);
    }
    let mut replacements = HashMap::new();
    for blob in refs {
        let value = match mode {
            FilesMode::Path => files
                .materialize(&blob)
                .await
                .map(|path| json!(path.display().to_string())),
            FilesMode::Url => files.locate(&blob).await.map(|location| match location {
                FileLocation::Url(url) => json!(url),
                FileLocation::Path(path) => json!(url::Url::from_file_path(&path)
                    .map(|url| url.to_string())
                    .unwrap_or_else(|()| path.display().to_string())),
            }),
            FilesMode::Base64 => files
                .get(&blob)
                .await
                .map(|bytes| json!(STANDARD.encode(bytes))),
            FilesMode::Off => unreachable!(),
        }
        .map_err(|e| format!("{blob} can't be passed to the tool: {e}"))?;
        replacements.insert(blob, value);
    }
    Ok(substitute_refs(input, &replacements))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dive_core::files::{BlobRef, FileError};
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        put: Mutex<Vec<(Vec<u8>, String)>>,
    }

    const HASH: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[async_trait]
    impl BlobStore for Fake {
        async fn put(&self, bytes: Vec<u8>, mime: &str) -> Result<StoredFile, FileError> {
            let size = bytes.len() as u64;
            self.put.lock().unwrap().push((bytes, mime.to_string()));
            Ok(StoredFile {
                name: None,
                blob: BlobRef::new(HASH, mime),
                mime: mime.to_string(),
                bytes: size,
                width: None,
                height: None,
            })
        }

        async fn put_as(&self, bytes: Vec<u8>, extension: &str) -> Result<StoredFile, FileError> {
            self.put(bytes, dive_core::files::mime_for(extension)).await
        }

        async fn get(&self, _blob: &BlobRef) -> Result<Vec<u8>, FileError> {
            Ok(b"png!".to_vec())
        }

        async fn delete(&self, _blob: &BlobRef) -> Result<(), FileError> {
            Ok(())
        }

        async fn locate(&self, blob: &BlobRef) -> Result<FileLocation, FileError> {
            Ok(FileLocation::Url(format!(
                "https://files.test/{}",
                blob.file_name()
            )))
        }

        async fn materialize(&self, blob: &BlobRef) -> Result<PathBuf, FileError> {
            Ok(PathBuf::from("/cache").join(blob.file_name()))
        }
    }

    fn blob() -> BlobRef {
        BlobRef::new(HASH, "image/png")
    }

    #[tokio::test]
    async fn refs_in_arguments_become_paths_links_or_contents_per_mode() {
        let files = Fake::default();
        let input = json!({"image": blob().to_string(), "note": "keep"});
        let as_path = pass(input.clone(), &files, FilesMode::Path).await.unwrap();
        assert_eq!(as_path["image"], json!(format!("/cache/{HASH}.png")));
        assert_eq!(as_path["note"], json!("keep"));
        let as_url = pass(input.clone(), &files, FilesMode::Url).await.unwrap();
        assert_eq!(
            as_url["image"],
            json!(format!("https://files.test/{HASH}.png"))
        );
        let as_base64 = pass(input.clone(), &files, FilesMode::Base64)
            .await
            .unwrap();
        assert_eq!(as_base64["image"], json!(STANDARD.encode(b"png!")));
        let off = pass(input.clone(), &files, FilesMode::Off).await.unwrap();
        assert_eq!(off, input);
    }

    #[tokio::test]
    async fn images_audio_and_embedded_blobs_are_stored_and_listed_with_the_result() {
        let files = Fake::default();
        let content = vec![
            ContentBlock::text("captured"),
            ContentBlock::image(STANDARD.encode(b"img"), "image/png"),
            ContentBlock::audio(STANDARD.encode(b"snd"), "audio/wav"),
            ContentBlock::resource(ResourceContents::BlobResourceContents {
                uri: "shot://1".into(),
                mime_type: Some("image/jpeg".into()),
                blob: STANDARD.encode(b"jpg"),
                meta: None,
            }),
            ContentBlock::image("not base64!", "image/png"),
        ];
        let (found, links) = binaries(&content);
        assert!(links.is_empty());
        let stored = store(&files, found).await;
        assert_eq!(stored.len(), 3);
        let put = files.put.lock().unwrap().clone();
        assert_eq!(
            put,
            vec![
                (b"img".to_vec(), "image/png".to_string()),
                (b"snd".to_vec(), "audio/wav".to_string()),
                (b"jpg".to_vec(), "image/jpeg".to_string()),
            ]
        );
        let result = attach(json!({"text": "captured"}), stored);
        assert_eq!(result["text"], json!("captured"));
        assert_eq!(result["files"][0]["ref"], json!(blob().to_string()));
        assert_eq!(result["files"][0]["mime"], json!("image/png"));
        assert_eq!(result["files"][1]["mime"], json!("audio/wav"));
    }

    #[test]
    fn results_that_are_not_objects_or_already_say_files_are_wrapped() {
        let stored = || {
            vec![StoredFile {
                name: None,
                blob: blob(),
                mime: "image/png".into(),
                bytes: 3,
                width: Some(1),
                height: Some(1),
            }]
        };
        let wrapped = attach(json!(["a"]), stored());
        assert_eq!(wrapped["result"], json!(["a"]));
        assert_eq!(wrapped["files"][0]["width"], json!(1));
        let clash = attach(json!({"files": 2}), stored());
        assert_eq!(clash["result"], json!({"files": 2}));
        assert_eq!(attach(json!({"a": 1}), Vec::new()), json!({"a": 1}));
    }

    #[test]
    fn resource_links_of_every_type_are_fetched() {
        let link = |uri: &str, mime: Option<&str>| {
            let mut resource = rmcp::model::Resource::new(uri, "file");
            resource.mime_type = mime.map(str::to_string);
            ContentBlock::resource_link(resource)
        };
        let (_, links) = binaries(&[
            link("file:///shot.png", Some("image/png")),
            link("file:///rows.csv", Some("text/csv")),
            link("file:///blob", None),
        ]);
        let uris: Vec<&str> = links.iter().map(|link| link.uri.as_str()).collect();
        assert_eq!(
            uris,
            ["file:///shot.png", "file:///rows.csv", "file:///blob"]
        );
    }

    #[tokio::test]
    async fn embedded_text_resources_are_stored_with_their_type() {
        let files = Fake::default();
        let text = |mime: Option<&str>| {
            ContentBlock::resource(ResourceContents::TextResourceContents {
                uri: "mem://1".into(),
                mime_type: mime.map(str::to_string),
                text: "a,b\n1,2\n".into(),
                meta: None,
            })
        };
        let (found, _) = binaries(&[text(Some("text/csv")), text(None)]);
        store(&files, found).await;
        let put = files.put.lock().unwrap().clone();
        assert_eq!(
            put,
            vec![
                (b"a,b\n1,2\n".to_vec(), "text/csv".to_string()),
                (b"a,b\n1,2\n".to_vec(), "text/plain".to_string()),
            ]
        );
    }
}
