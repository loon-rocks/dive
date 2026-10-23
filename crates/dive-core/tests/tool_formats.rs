use dive_core::format::{TOOL_FORMAT, TOOL_FORMAT_OLDEST};
use dive_core::user_tools::{parse_tool_source, validate_tool};
use std::path::{Path, PathBuf};

fn fixture_dir(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/tools/v{version}"))
}

fn fixtures_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    paths.sort();
    paths
}

fn declared(path: &Path) -> u64 {
    let raw = std::fs::read_to_string(path).unwrap();
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
    value["version"].as_u64().unwrap_or(1)
}

fn load(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap();
    let doc = parse_tool_source(&raw)
        .unwrap_or_else(|e| panic!("{} no longer loads: {e}", path.display()));
    validate_tool(&doc).unwrap_or_else(|e| panic!("{} no longer validates: {e}", path.display()));
    serde_json::to_value(&doc).unwrap()
}

#[test]
fn every_tool_version_has_a_fixture_directory() {
    for version in TOOL_FORMAT_OLDEST..=TOOL_FORMAT {
        let dir = fixture_dir(version);
        assert!(
            dir.is_dir(),
            "tool format {version} has no fixtures under {}",
            dir.display()
        );
        assert!(
            !fixtures_in(&dir).is_empty(),
            "tool format {version} has an empty fixture directory"
        );
    }
}

#[test]
fn every_tool_fixture_at_every_format_still_loads() {
    for version in TOOL_FORMAT_OLDEST..=TOOL_FORMAT {
        for path in fixtures_in(&fixture_dir(version)) {
            assert_eq!(
                declared(&path),
                u64::from(version),
                "{} sits in the v{version} directory but declares another version",
                path.display()
            );
            load(&path);
        }
    }
}

#[test]
fn tool_golden_pairs_load_identically_across_formats() {
    for version in (TOOL_FORMAT_OLDEST..TOOL_FORMAT).filter(|v| *v < TOOL_FORMAT) {
        for older in fixtures_in(&fixture_dir(version)) {
            let newer = fixture_dir(version + 1).join(older.file_name().unwrap());
            if !newer.exists() {
                continue;
            }
            assert_eq!(
                load(&older),
                load(&newer),
                "{} and {} must load to the same tool",
                older.display(),
                newer.display()
            );
        }
    }
}

#[test]
fn a_newer_tool_format_is_refused_before_the_schema_is_consulted() {
    let raw = format!(
        "version: {}\nname: future\ndescription: d\nkind: reshape\nkey_from_the_future: 1\n",
        TOOL_FORMAT + 1
    );
    let err = parse_tool_source(&raw).unwrap_err().to_string();
    assert!(
        err.contains(&format!("tool version {}", TOOL_FORMAT + 1)),
        "{err}"
    );
    assert!(!err.contains("unknown field"), "{err}");
}

#[test]
fn an_env_entry_is_a_string_or_a_value_with_a_description() {
    let path = fixture_dir(3).join("hubspot_contacts.yaml");
    let raw = std::fs::read_to_string(&path).unwrap();
    let doc = parse_tool_source(&raw).unwrap();
    let dive_core::user_tools::ToolKind::Exec { env, .. } = &doc.kind else {
        panic!("hubspot_contacts is an exec tool");
    };
    assert_eq!(
        env["HUBSPOT_URL"].value(),
        "${HUBSPOT_URL:-https://api.hubapi.com}"
    );
    assert_eq!(env["HUBSPOT_URL"].description(), None);
    assert_eq!(env["HUBSPOT_TOKEN"].value(), "${HUBSPOT_TOKEN}");
    assert!(env["HUBSPOT_TOKEN"]
        .description()
        .unwrap()
        .contains("https://app.hubspot.com/private-apps"));

    let written = serde_yaml::to_string(&doc).unwrap();
    let again = parse_tool_source(&written).unwrap();
    assert_eq!(
        serde_json::to_value(&doc).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
    let value = serde_json::to_value(&doc).unwrap();
    assert_eq!(
        value["env"]["HUBSPOT_URL"],
        "${HUBSPOT_URL:-https://api.hubapi.com}"
    );
    assert_eq!(value["env"]["HUBSPOT_TOKEN"]["value"], "${HUBSPOT_TOKEN}");
}

#[test]
fn an_env_entry_object_needs_a_value_and_takes_no_other_keys() {
    let base = "version: 3\nname: t\ndescription: d\nkind: exec\ncommand: echo\nenv:\n  KEY:\n";
    let unknown = parse_tool_source(&format!(
        "{base}    value: \"${{KEY}}\"\n    link: https://x\n"
    ))
    .unwrap_err();
    assert!(unknown.contains("env.KEY.link"), "{unknown}");
    let no_value = parse_tool_source(&format!("{base}    description: d\n")).unwrap_err();
    assert!(no_value.contains("value"), "{no_value}");
}

#[test]
fn a_tool_v2_file_with_plain_env_strings_still_loads() {
    let raw = std::fs::read_to_string(fixture_dir(2).join("web_search.yaml")).unwrap();
    let doc = parse_tool_source(&raw).unwrap();
    let needed = dive_core::user_tools::needed_secrets(&doc);
    assert_eq!(
        needed,
        [dive_core::secrets::NeededSecret {
            name: "SEARCH_TOKEN".to_string(),
            description: None
        }]
    );
}
