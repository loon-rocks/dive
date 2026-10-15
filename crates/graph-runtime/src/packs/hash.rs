use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const HASH_PREFIX: &str = "sha256:";

pub fn content_hash(dir: &Path) -> std::io::Result<String> {
    let mut files = Vec::new();
    collect(dir, Path::new(""), &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        let content = std::fs::read(dir.join(&relative))?;
        let name = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{HASH_PREFIX}{hex}"))
}

fn collect(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let name = entry.file_name();
        if relative.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let path = relative.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, path: &str, content: &str) {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn the_hash_covers_paths_and_contents_and_skips_git() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pack.toml", "version = 1\n");
        write(dir.path(), "tools/a.yaml", "a");
        let first = content_hash(dir.path()).unwrap();
        assert!(first.starts_with("sha256:"));
        assert_eq!(first.len(), "sha256:".len() + 64);

        write(dir.path(), ".git/HEAD", "ref: refs/heads/main");
        assert_eq!(content_hash(dir.path()).unwrap(), first);

        write(dir.path(), "tools/a.yaml", "b");
        let edited = content_hash(dir.path()).unwrap();
        assert_ne!(edited, first);

        write(dir.path(), "tools/a.yaml", "a");
        std::fs::rename(dir.path().join("tools"), dir.path().join("plans")).unwrap();
        assert_ne!(content_hash(dir.path()).unwrap(), first);
    }
}
