use super::{BlobRef, BlobStore, FileError, FileLocation, StoredFile};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct Memory(Mutex<HashMap<String, Vec<u8>>>);

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
