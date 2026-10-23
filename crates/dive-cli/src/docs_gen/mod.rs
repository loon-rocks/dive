mod catalog;
mod cli;
mod content;
mod formats;
mod mdx;
mod variables;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const UPDATE_ENV: &str = "DIVE_UPDATE_DOCS";

pub struct Section {
    pub dir: Option<&'static str>,
    pub nav: Option<String>,
    pub hand_written: &'static [&'static str],
    pub files: BTreeMap<String, String>,
}

fn sections() -> Vec<Section> {
    let mut sections = vec![cli::section()];
    sections.extend(catalog::sections());
    sections.extend(formats::sections());
    sections
}

fn docs_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

fn existing_files(dir: &Path, root: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            existing_files(&path, root, found);
        } else if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("mdx" | "json")
        ) {
            let relative = path.strip_prefix(root).unwrap();
            found.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_and_argument_has_help_text() {
        let missing = cli::missing_help();
        assert!(
            missing.is_empty(),
            "the CLI reference is generated from clap's help text, so every visible command and argument needs one:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn the_generated_reference_matches_the_code() {
        let root = docs_root();
        let update = std::env::var(UPDATE_ENV).is_ok_and(|value| value == "1");
        let nav = std::fs::read_to_string(root.join("docs.json")).unwrap();
        let mut problems = Vec::new();
        for section in sections() {
            for (path, content) in &section.files {
                let file = root.join(path);
                if update {
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::fs::write(&file, content).unwrap();
                } else if std::fs::read_to_string(&file).ok().as_deref() != Some(content.as_str()) {
                    problems.push(format!("docs/{path} is out of date"));
                }
            }
            if let Some(dir) = section.dir {
                let mut existing = Vec::new();
                existing_files(&root.join(dir), &root, &mut existing);
                for path in existing {
                    if section.files.contains_key(&path)
                        || section.hand_written.contains(&path.as_str())
                    {
                        continue;
                    }
                    if update {
                        std::fs::remove_file(root.join(&path)).unwrap();
                    } else {
                        problems.push(format!("docs/{path} has nothing in the code behind it"));
                    }
                }
            }
            match &section.nav {
                Some(nav_file) => {
                    if !nav.contains(&format!("\"$ref\": \"{nav_file}\"")) {
                        problems.push(format!(
                            "docs/docs.json must load {nav_file} with {{\"$ref\": \"{nav_file}\"}}"
                        ));
                    }
                }
                None => {
                    for path in section
                        .files
                        .keys()
                        .filter(|path| path.ends_with(".mdx") && !path.starts_with("snippets/"))
                    {
                        let page = path.trim_end_matches(".mdx");
                        if !nav.contains(&format!("\"{page}\"")) {
                            problems.push(format!("docs/docs.json does not list \"{page}\""));
                        }
                    }
                }
            }
        }
        let with_variables = variables::apply(&nav);
        if with_variables != nav {
            if update {
                std::fs::write(root.join("docs.json"), &with_variables).unwrap();
            } else {
                problems.push("docs/docs.json variables are out of date".to_string());
            }
        }
        problems.extend(content::problems());
        assert!(
            problems.is_empty(),
            "{}\nRegenerate with `mise run docs:generate`.",
            problems.join("\n")
        );
    }
}
