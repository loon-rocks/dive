use super::mdx::{frontmatter, paragraphs, schema_fields, text_line, Field};
use super::Section;
use graph_core::schemas;
use serde_json::json;
use serde_yaml::Value as Yaml;
use std::collections::BTreeMap;
use std::fmt::Write as _;

const TOOL_FILE: &str = "tools/reference/tool-file";
const CONFIG_PAGE: &str = "reference/configuration";
const ENV_PAGE: &str = "reference/environment-variables";
const ENV_SNIPPETS: &str = "snippets/generated/environment";
const CONFIG_SNIPPETS: &str = "snippets/generated/config";

struct Format {
    source: &'static str,
    slug: &'static str,
    page: &'static str,
}

const FORMATS: &[Format] = &[
    Format {
        source: schemas::AGENT,
        slug: "agent",
        page: "agents/reference/agent-file",
    },
    Format {
        source: schemas::PLAN,
        slug: "plan",
        page: "plans/reference/plan-file",
    },
    Format {
        source: schemas::TOOL,
        slug: "tool",
        page: TOOL_FILE,
    },
    Format {
        source: graph_config::SCHEMA,
        slug: "config",
        page: CONFIG_PAGE,
    },
];

pub fn sections() -> Vec<Section> {
    let mut pages = BTreeMap::new();
    let mut published = BTreeMap::new();
    let mut tool_kinds = BTreeMap::new();
    let mut config_snippets = BTreeMap::new();
    for format in FORMATS {
        let schema: Yaml = serde_yaml::from_str(format.source).expect("file schemas parse");
        if format.page == CONFIG_PAGE {
            for snippet in config_sections(&schema) {
                config_snippets.insert(snippet.path, snippet.content);
            }
        }
        let target = if format.page == TOOL_FILE {
            &mut tool_kinds
        } else {
            &mut pages
        };
        target.insert(format!("{}.mdx", format.page), page(format, &schema));
        let mut json =
            serde_json::to_string_pretty(&serde_json::to_value(&schema).unwrap()).unwrap();
        json.push('\n');
        published.insert(format!("schemas/{}.schema.json", format.slug), json);
        if let Some(branches) = schema.get("oneOf").and_then(Yaml::as_sequence) {
            for branch in branches {
                let kind = text(branch, "title");
                tool_kinds.insert(
                    format!("{TOOL_FILE}/{kind}.mdx"),
                    kind_page(format, &schema, branch),
                );
            }
        }
    }
    let kinds: Vec<String> = tool_kinds
        .keys()
        .map(|path| path.trim_end_matches(".mdx").to_string())
        .filter(|path| path != TOOL_FILE)
        .collect();
    let ordered: Vec<serde_json::Value> = ["exec", "prompt", "reshape", "decision"]
        .iter()
        .map(|kind| format!("{TOOL_FILE}/{kind}"))
        .filter(|path| kinds.contains(path))
        .chain(
            kinds
                .iter()
                .filter(|path| {
                    !["exec", "prompt", "reshape", "decision"]
                        .iter()
                        .any(|kind| path.ends_with(&format!("/{kind}")))
                })
                .cloned(),
        )
        .map(|path| json!(path))
        .collect();
    let mut nav = serde_json::to_string_pretty(
        &json!({ "group": "Tool file", "root": TOOL_FILE, "pages": ordered }),
    )
    .unwrap();
    nav.push('\n');
    tool_kinds.insert(format!("{TOOL_FILE}/navigation.json"), nav);
    vec![
        Section {
            dir: None,
            nav: None,
            hand_written: &[],
            files: pages,
        },
        Section {
            dir: Some(TOOL_FILE),
            nav: Some(format!("{TOOL_FILE}/navigation.json")),
            hand_written: &[],
            files: tool_kinds,
        },
        Section {
            dir: Some(CONFIG_SNIPPETS),
            nav: None,
            hand_written: &[],
            files: config_snippets,
        },
        Section {
            dir: Some("schemas"),
            nav: None,
            hand_written: &[],
            files: published,
        },
        environment(),
    ]
}

fn text<'a>(schema: &'a Yaml, key: &str) -> &'a str {
    schema.get(key).and_then(Yaml::as_str).unwrap_or_default()
}

fn file_facts(out: &mut String, format: &Format, schema: &Yaml) {
    let kind = text(schema, "x-format");
    let locations: Vec<String> = schema
        .get("x-locations")
        .and_then(Yaml::as_sequence)
        .map(|items| {
            items
                .iter()
                .filter_map(Yaml::as_str)
                .map(text_line)
                .collect()
        })
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "| File type | Location |\n| --- | --- |\n| {kind} | {} |\n",
        locations.join("<br />")
    );
    if kind == "YAML" {
        let _ = writeln!(
            out,
            "Unknown keys are a load error. For completion in your editor, start the file with `# yaml-language-server: $schema=https://raw.githubusercontent.com/tylerdavis/graph/main/docs/schemas/{}.schema.json`.\n",
            format.slug
        );
    }
}

fn example_block(format: &Format, example: &Yaml) -> (&'static str, String) {
    match text_kind(format) {
        "TOML" => {
            let json = serde_json::to_value(example).unwrap();
            ("toml", toml::to_string(&json).unwrap_or_default())
        }
        _ => ("yaml", serde_yaml::to_string(example).unwrap()),
    }
}

fn text_kind(format: &Format) -> &'static str {
    if format.slug == "config" {
        "TOML"
    } else {
        "YAML"
    }
}

fn panel(out: &mut String, format: &Format, example: Option<&Yaml>) {
    let Some(example) = example else {
        return;
    };
    out.push_str("<RequestExample>\n\n");
    let (language, code) = example_block(format, example);
    let _ = writeln!(out, "```{language} Example\n{}\n```\n", code.trim_end());
    out.push_str("</RequestExample>\n");
}

fn first_example(node: &Yaml) -> Option<&Yaml> {
    node.get("examples")
        .and_then(Yaml::as_sequence)
        .and_then(|examples| examples.first())
}

fn page(format: &Format, schema: &Yaml) -> String {
    let title = text(schema, "title");
    let mut out = String::new();
    frontmatter(
        &mut out,
        &[
            ("title", title.to_string()),
            ("description", text(schema, "description").to_string()),
            ("playground", "none".to_string()),
        ],
        &[title, "schema", text(schema, "x-format")],
    );
    let config = (format.page == CONFIG_PAGE).then(|| config_sections(schema));
    if let Some(snippets) = &config {
        for snippet in snippets {
            let _ = writeln!(
                out,
                "import {} from \"/{}\";",
                snippet.component, snippet.path
            );
        }
        out.push('\n');
    }
    file_facts(&mut out, format, schema);
    let branches = schema.get("oneOf").and_then(Yaml::as_sequence);
    out.push_str(if branches.is_some() {
        "## Common fields\n\n"
    } else {
        "## Fields\n\n"
    });
    match &config {
        Some(snippets) => {
            for snippet in snippets {
                let _ = writeln!(out, "<{} />\n", snippet.component);
            }
        }
        None => schema_fields(&mut out, schema, Field::Param, ""),
    }
    if let Some(branches) = branches {
        out.push_str("## Kinds\n\nEach `kind` adds its own fields.\n\n");
        for branch in branches {
            let kind = text(branch, "title");
            let _ = writeln!(
                out,
                "- [`{kind}`](/{TOOL_FILE}/{kind}): {}",
                paragraphs(&super::mdx::first_sentence(text(branch, "description")))
            );
        }
        out.push('\n');
    }
    let example = first_example(schema).or_else(|| {
        branches
            .and_then(|branches| branches.first())
            .and_then(first_example)
    });
    panel(&mut out, format, example);
    out
}

fn kind_page(format: &Format, schema: &Yaml, branch: &Yaml) -> String {
    let kind = text(branch, "title");
    let mut out = String::new();
    frontmatter(
        &mut out,
        &[
            ("title", format!("{kind} tools")),
            ("sidebarTitle", format!("kind: {kind}")),
            (
                "description",
                super::mdx::first_sentence(text(branch, "description")),
            ),
            ("playground", "none".to_string()),
        ],
        &[kind, "tool file", text(schema, "x-format")],
    );
    let _ = writeln!(out, "{}\n", paragraphs(text(branch, "description")));
    let _ = writeln!(
        out,
        "These fields go alongside the [common fields](/{TOOL_FILE}#common-fields), with `kind: {kind}`.\n"
    );
    let mut fields = branch.clone();
    if let Some(props) = fields.get_mut("properties").and_then(Yaml::as_mapping_mut) {
        props.shift_remove("kind");
    }
    out.push_str("## Fields\n\n");
    schema_fields(&mut out, &fields, Field::Param, "");
    panel(&mut out, format, first_example(branch));
    out
}

fn config_anchor(path: &str) -> String {
    let slug: String = path
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("/reference/configuration#param-{slug}")
}

struct Snippet {
    path: String,
    component: String,
    content: String,
}

fn config_sections(schema: &Yaml) -> Vec<Snippet> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Yaml::as_sequence)
        .map(|names| names.iter().filter_map(Yaml::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Yaml::as_mapping) else {
        return Vec::new();
    };
    properties
        .iter()
        .filter_map(|(name, property)| {
            let name = name.as_str()?;
            let mut single = serde_yaml::Mapping::new();
            single.insert(name.into(), property.clone());
            let mut sub = serde_yaml::Mapping::new();
            sub.insert("properties".into(), Yaml::Mapping(single));
            if required.contains(&name) {
                sub.insert("required".into(), Yaml::Sequence(vec![name.into()]));
            }
            let mut content = String::new();
            schema_fields(&mut content, &Yaml::Mapping(sub), Field::Param, "");
            let slug = name.replace('_', "-");
            Some(Snippet {
                path: format!("{CONFIG_SNIPPETS}/{slug}.mdx"),
                component: import_name("Config", &slug),
                content: format!("{}\n", content.trim_end()),
            })
        })
        .collect()
}

fn import_name(prefix: &str, slug: &str) -> String {
    let mut name = String::from(prefix);
    let mut upper = true;
    for ch in slug.chars() {
        if ch.is_ascii_alphanumeric() {
            if upper {
                name.push(ch.to_ascii_uppercase());
            } else {
                name.push(ch);
            }
            upper = false;
        } else {
            upper = true;
        }
    }
    name
}

fn env_field(variable: &Yaml) -> String {
    let name = text(variable, "name");
    let values = variable.get("values").and_then(Yaml::as_mapping);
    let kind = if values.is_some() {
        "enum<string>".to_string()
    } else {
        text(variable, "type").to_string()
    };
    let mut out = format!("<ParamField header=\"{name}\" type=\"{kind}\"");
    if let Some(default) = variable.get("default").and_then(Yaml::as_str) {
        let _ = write!(out, " default=\"{}\"", default.replace('"', "&quot;"));
    }
    out.push_str(">\n");
    let _ = writeln!(out, "  {}", text_line(text(variable, "description")));
    if let Some(path) = variable.get("overrides").and_then(Yaml::as_str) {
        let _ = writeln!(
            out,
            "\n  Overrides [`{path}`]({}) in config.toml.",
            config_anchor(path)
        );
    }
    if let Some(values) = values {
        out.push_str("\n  | Value | Effect |\n  | --- | --- |\n");
        for (value, effect) in values {
            let _ = writeln!(
                out,
                "  | `{}` | {} |",
                value.as_str().unwrap_or_default(),
                text_line(effect.as_str().unwrap_or_default())
            );
        }
        out.push('\n');
    }
    out.push_str("</ParamField>\n");
    out
}

fn env_slug(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

fn environment() -> Section {
    let schema: Yaml = serde_yaml::from_str(graph_config::SCHEMA).expect("config schema parses");
    let groups = schema
        .get("x-environment")
        .and_then(Yaml::as_sequence)
        .cloned()
        .unwrap_or_default();
    let mut files = BTreeMap::new();
    let mut imports = String::new();
    let mut body = String::new();
    for group in &groups {
        let _ = writeln!(body, "## {}\n", text(group, "group"));
        for variable in group
            .get("variables")
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
        {
            let slug = env_slug(text(variable, "name"));
            files.insert(format!("{ENV_SNIPPETS}/{slug}.mdx"), env_field(variable));
            let component = import_name("Env", &slug);
            let _ = writeln!(
                imports,
                "import {component} from \"/{ENV_SNIPPETS}/{slug}.mdx\";"
            );
            let _ = writeln!(body, "<{component} />\n");
        }
    }
    let mut page = String::new();
    frontmatter(
        &mut page,
        &[
            ("title", "Environment variables".to_string()),
            ("playground", "none".to_string()),
        ],
        &["environment variables", "env", "GRAPH_LOG", "OTEL"],
    );
    let _ = writeln!(page, "{imports}");
    page.push_str(body.trim_end());
    page.push('\n');
    files.insert(format!("{ENV_PAGE}.mdx"), page);
    Section {
        dir: Some(ENV_SNIPPETS),
        nav: None,
        hand_written: &[],
        files,
    }
}
