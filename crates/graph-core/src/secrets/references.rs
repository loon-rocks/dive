use super::{env_references, is_secret_name};
use crate::user_tools::{ToolKind, UserToolDoc};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Config,
    Tool,
    PackTool,
    PackRequires,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reference {
    pub name: String,
    pub kind: ReferenceKind,
    pub file: String,
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
    pub has_default: bool,
}

impl Reference {
    pub fn describe(&self) -> String {
        match (&self.kind, &self.pack) {
            (ReferenceKind::PackRequires, Some(pack)) => {
                format!("pack {pack} requires.env ({})", self.file)
            }
            (ReferenceKind::PackTool, Some(pack)) => {
                format!("pack {pack}: {} ({})", self.file, self.field)
            }
            _ => format!("{} ({})", self.file, self.field),
        }
    }
}

pub fn in_config(path: &Path, text: &str) -> Result<Vec<Reference>, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|error| format!("parsing {}: {error}", path.display()))?;
    let mut found = Vec::new();
    walk_toml(&toml::Value::Table(table), "", &mut |field, text| {
        for reference in env_references(text) {
            let name = match reference.default {
                Some(default) => format!("{}:-{default}", reference.name),
                None => reference.name.to_string(),
            };
            if is_secret_name(&name) {
                found.push(Reference {
                    name,
                    kind: ReferenceKind::Config,
                    file: path.display().to_string(),
                    field: field.to_string(),
                    pack: None,
                    has_default: false,
                });
            }
        }
    });
    Ok(found)
}

fn walk_toml(value: &toml::Value, path: &str, visit: &mut dyn FnMut(&str, &str)) {
    let child = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    match value {
        toml::Value::String(text) => visit(path, text),
        toml::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk_toml(item, &child(&index.to_string()), visit);
            }
        }
        toml::Value::Table(table) => {
            for (key, item) in table {
                walk_toml(item, &child(key), visit);
            }
        }
        _ => {}
    }
}

pub fn in_tool(doc: &UserToolDoc, pack: Option<&str>) -> Vec<Reference> {
    let ToolKind::Exec { env, .. } = &doc.kind else {
        return Vec::new();
    };
    let file = doc
        .path
        .as_ref()
        .map_or_else(|| doc.name.clone(), |path| path.display().to_string());
    let mut found = Vec::new();
    for (key, value) in env {
        for reference in env_references(value) {
            if !is_secret_name(reference.name) {
                continue;
            }
            found.push(Reference {
                name: reference.name.to_string(),
                kind: if pack.is_some() {
                    ReferenceKind::PackTool
                } else {
                    ReferenceKind::Tool
                },
                file: file.clone(),
                field: format!("env.{key}"),
                pack: pack.map(str::to_string),
                has_default: reference.default.is_some(),
            });
        }
    }
    found
}

pub fn in_requires(pack: &str, manifest: &Path, names: &[String]) -> Vec<Reference> {
    names
        .iter()
        .filter(|name| is_secret_name(name))
        .map(|name| Reference {
            name: name.clone(),
            kind: ReferenceKind::PackRequires,
            file: manifest.display().to_string(),
            field: "requires.env".to_string(),
            pack: Some(pack.to_string()),
            has_default: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_references_carry_their_dotted_field() {
        let found = in_config(
            Path::new("config.toml"),
            "[providers.anthropic]\napi_key = \"${ANTHROPIC_API_KEY}\"\n[mcp.linear]\nheaders = { Authorization = \"Bearer ${LINEAR_API_KEY}\" }\n[settings]\ndata_dir = \"${HOME_DIR:-x}\"\n",
        )
        .unwrap();
        let summary: Vec<(&str, &str)> = found
            .iter()
            .map(|reference| (reference.name.as_str(), reference.field.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("LINEAR_API_KEY", "mcp.linear.headers.Authorization"),
                ("ANTHROPIC_API_KEY", "providers.anthropic.api_key"),
            ]
        );
    }

    #[test]
    fn tool_references_note_defaults_and_the_pack() {
        let doc = crate::user_tools::parse_tool_source(
            "name: search\ndescription: x\nkind: exec\ncommand: echo\nenv:\n  URL: \"${SEARCH_URL:-https://x}\"\n  KEY: \"${SEARCH_KEY}\"\n",
        )
        .unwrap();
        let found = in_tool(&doc, Some("web"));
        assert_eq!(found.len(), 2);
        let key = found.iter().find(|r| r.name == "SEARCH_KEY").unwrap();
        assert_eq!(
            (key.kind, key.field.as_str(), key.has_default),
            (ReferenceKind::PackTool, "env.KEY", false)
        );
        assert!(found
            .iter()
            .any(|r| r.name == "SEARCH_URL" && r.has_default));
    }
}
