use graph_core::packs::requirements::GRAPH_VERSION;
use std::path::{Path, PathBuf};

pub const FILES: &[(&str, &str)] = &[
    ("pack.toml", include_str!("scaffold/pack.toml")),
    (
        "tools/word_count.yaml",
        include_str!("scaffold/word_count.yaml"),
    ),
    (
        "plans/count_words.yaml",
        include_str!("scaffold/count_words.yaml"),
    ),
    (
        "agents/word_counter.yaml",
        include_str!("scaffold/word_counter.yaml"),
    ),
    ("README.md", include_str!("scaffold/README.md")),
    ("CHANGELOG.md", include_str!("scaffold/CHANGELOG.md")),
    ("MIGRATING.md", include_str!("scaffold/MIGRATING.md")),
];

pub const WORKFLOW: &str = ".github/workflows/packs.yaml";

pub const REPO_FILES: &[(&str, &str)] = &[
    (WORKFLOW, include_str!("scaffold/workflow.yaml")),
    (".gitignore", include_str!("scaffold/gitignore")),
];

pub fn graph_requirement() -> String {
    let mut parts = GRAPH_VERSION.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    format!(">={major}.{minor}")
}

pub fn render(text: &str, name: &str) -> String {
    text.replace("@NAME@", name)
        .replace("@GRAPH@", &graph_requirement())
        .replace("@VERSION@", GRAPH_VERSION)
}

fn write_file(target: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(target, text)
}

pub fn write(dir: &Path, name: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (path, text) in FILES {
        let target = dir.join(path);
        write_file(&target, &render(text, name))?;
        written.push(target);
    }
    Ok(written)
}

pub fn write_repo(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (path, text) in REPO_FILES {
        let target = root.join(path);
        if target.exists() {
            continue;
        }
        write_file(&target, &render(text, ""))?;
        written.push(target);
    }
    Ok(written)
}
