const KEY: &str = "\n  \"variables\": {\n";
const CLOSE: &str = "\n  }";

fn values() -> Vec<(&'static str, u32)> {
    vec![
        ("config_file_version", dive_config::CONFIG_FORMAT),
        ("plan_file_version", dive_core::format::PLAN_FORMAT),
        ("tool_file_version", dive_core::format::TOOL_FORMAT),
        ("agent_file_version", dive_core::format::AGENT_FORMAT),
        ("data_directory_version", dive_store::STORE_FORMAT),
        ("pack_file_version", dive_config::pack::PACK_FORMAT),
        ("pack_lock_version", dive_config::pack::LOCK_FORMAT),
    ]
}

pub fn apply(docs_json: &str) -> String {
    let start = docs_json
        .find(KEY)
        .expect("docs.json has a top-level variables object")
        + KEY.len();
    let end = start
        + docs_json[start..]
            .find(CLOSE)
            .expect("docs.json variables object closes");
    let generated = values();
    let mut entries: Vec<String> = docs_json[start..end]
        .lines()
        .map(|line| line.trim().trim_end_matches(',').to_string())
        .filter(|entry| {
            !entry.is_empty()
                && !generated
                    .iter()
                    .any(|(name, _)| entry.starts_with(&format!("\"{name}\"")))
        })
        .collect();
    entries.extend(
        generated
            .iter()
            .map(|(name, value)| format!("\"{name}\": \"{value}\"")),
    );
    let body: Vec<String> = entries.iter().map(|entry| format!("    {entry}")).collect();
    format!(
        "{}{}{}",
        &docs_json[..start],
        body.join(",\n"),
        &docs_json[end..]
    )
}
