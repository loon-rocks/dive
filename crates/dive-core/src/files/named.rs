use super::{natural_cmp, BlobStore, FileDecls};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const FILES_ROOT: &str = "files";

pub fn files_root(decls: &FileDecls, dir: &Path) -> Value {
    let shown = dir.display().to_string();
    let entries: Map<String, Value> = decls
        .iter()
        .map(|(key, decl)| {
            let entry = if decl.is_glob() {
                json!({ "dir": shown })
            } else {
                json!({ "dir": shown, "path": dir.join(&decl.name).display().to_string() })
            };
            (key.clone(), entry)
        })
        .collect();
    Value::Object(entries)
}

pub fn files_schema(decls: &FileDecls) -> Value {
    let file = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "ref": {"type": "string", "format": "blob"},
            "mime": {"type": "string"},
            "bytes": {"type": "integer"}
        }
    });
    let named: Map<String, Value> = decls
        .iter()
        .map(|(key, decl)| {
            let entry = if decl.is_glob() {
                json!({"type": "array", "items": file})
            } else {
                file.clone()
            };
            (key.clone(), entry)
        })
        .collect();
    json!({"type": "array", "items": file, "properties": named})
}

#[derive(Clone)]
pub struct FileSpace {
    pub store: Arc<dyn BlobStore>,
    pub dir: PathBuf,
}

impl FileSpace {
    pub fn prepare(&self, decls: &FileDecls) -> Result<NamedFiles, String> {
        let files = &self.dir;
        let dir = files.join("tmp").join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("couldn't make a directory for the call's files: {e}"))?;
        let ignore = files.join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(ignore, "*\n");
        }
        Ok(NamedFiles {
            decls: decls.clone(),
            dir,
            store: self.store.clone(),
        })
    }
}

pub struct NamedFiles {
    decls: FileDecls,
    dir: PathBuf,
    store: Arc<dyn BlobStore>,
}

impl NamedFiles {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn root(&self) -> Value {
        files_root(&self.decls, &self.dir)
    }

    pub fn scope(&self, scope: &Map<String, Value>) -> Map<String, Value> {
        let mut scope = scope.clone();
        scope.insert(FILES_ROOT.to_string(), self.root());
        scope
    }

    pub async fn collect(&self, result: Value) -> Result<Value, String> {
        let entries = collect_dir(&self.dir, &self.decls, self.store.as_ref(), false).await?;
        Ok(merge(result, entries))
    }
}

pub async fn collect_dir(
    dir: &Path,
    decls: &FileDecls,
    store: &dyn BlobStore,
    keep_undeclared: bool,
) -> Result<Vec<Value>, String> {
    let mut written: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading the call's files: {e}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    written.sort_by(|a, b| natural_cmp(a, b));
    if let Some((key, decl)) = decls
        .iter()
        .find(|(_, decl)| !decl.optional && !written.iter().any(|name| decl.matches(name)))
    {
        return Err(format!(
            "the tool didn't write the declared file \"{key}\" ({})",
            decl.name
        ));
    }
    let mut entries = Vec::new();
    for name in written {
        let declared = decls.iter().find(|(_, decl)| decl.matches(&name));
        if declared.is_none() && !keep_undeclared {
            continue;
        }
        let bytes = std::fs::read(dir.join(&name)).map_err(|e| format!("reading {name}: {e}"))?;
        let extension = name
            .rsplit_once('.')
            .map_or("bin", |(_, extension)| extension);
        let mut stored = store
            .put_as(bytes, extension)
            .await
            .map_err(|e| e.to_string())?;
        stored.name = declared.map(|(key, _)| key.clone());
        let mut entry = serde_json::to_value(&stored).map_err(|e| e.to_string())?;
        if declared.is_some_and(|(_, decl)| decl.is_glob()) {
            entry["many"] = json!(true);
        }
        entries.push(entry);
    }
    Ok(entries)
}

impl Drop for NamedFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn merge(result: Value, entries: Vec<Value>) -> Value {
    if entries.is_empty() {
        return result;
    }
    match result {
        Value::Object(mut map) => {
            let files = map
                .entry("files")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(listed) = files {
                for entry in entries {
                    match listed
                        .iter_mut()
                        .find(|item| item.get("ref") == entry.get("ref"))
                    {
                        Some(same) => *same = entry,
                        None => listed.push(entry),
                    }
                }
                Value::Object(map)
            } else {
                json!({ "result": Value::Object(map), "files": entries })
            }
        }
        other => json!({ "result": other, "files": entries }),
    }
}

#[cfg(test)]
#[path = "named_tests.rs"]
mod tests;
