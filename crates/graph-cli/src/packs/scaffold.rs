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
    (
        ".github/workflows/test.yaml",
        include_str!("scaffold/workflow.yaml"),
    ),
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

pub fn write(dir: &Path, name: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (path, text) in FILES {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, render(text, name))?;
        written.push(target);
    }
    Ok(written)
}
