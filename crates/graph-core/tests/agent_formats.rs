use graph_core::agent::doc::{global_fragments, parse_agent_source, validate_doc};
use graph_core::format::{AGENT_FORMAT, AGENT_FORMAT_OLDEST};
use std::path::{Path, PathBuf};

fn fixture_dir(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/agents/v{version}"))
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
    let doc = parse_agent_source(&raw)
        .unwrap_or_else(|e| panic!("{} no longer loads: {e}", path.display()));
    let fragments: Vec<&str> = global_fragments().into_keys().collect();
    let problems = validate_doc(&doc, &fragments);
    assert!(
        problems.is_empty(),
        "{} no longer validates: {problems:?}",
        path.display()
    );
    serde_json::to_value(&doc).unwrap()
}

#[test]
fn every_agent_version_has_a_fixture_directory() {
    for version in AGENT_FORMAT_OLDEST..=AGENT_FORMAT {
        let dir = fixture_dir(version);
        assert!(
            dir.is_dir(),
            "agent format {version} has no fixtures under {}",
            dir.display()
        );
        assert!(
            !fixtures_in(&dir).is_empty(),
            "agent format {version} has an empty fixture directory"
        );
    }
}

#[test]
fn every_agent_fixture_at_every_format_still_loads() {
    for version in AGENT_FORMAT_OLDEST..=AGENT_FORMAT {
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
fn agent_golden_pairs_load_identically_across_formats() {
    for version in AGENT_FORMAT_OLDEST..AGENT_FORMAT {
        for older in fixtures_in(&fixture_dir(version)) {
            let newer = fixture_dir(version + 1).join(older.file_name().unwrap());
            if !newer.exists() {
                continue;
            }
            assert_eq!(
                load(&older),
                load(&newer),
                "{} and {} must load to the same agent",
                older.display(),
                newer.display()
            );
        }
    }
}

#[test]
fn a_version_2_agent_declares_the_attachments_it_reads() {
    let raw = std::fs::read_to_string(fixture_dir(2).join("network_drafter.yaml")).unwrap();
    let doc = parse_agent_source(&raw).unwrap();
    assert_eq!(doc.attachments, ["network"]);
    let raw = std::fs::read_to_string(fixture_dir(1).join("chat.yaml")).unwrap();
    assert!(parse_agent_source(&raw).unwrap().attachments.is_empty());
}

#[test]
fn a_newer_agent_format_is_refused_before_the_schema_is_consulted() {
    let raw = format!(
        "version: {}\nname: future\ndescription: d\nmodel: chat\ntools: []\nkey_from_the_future: 1\nsystem_prompt: p\n",
        AGENT_FORMAT + 1
    );
    let err = parse_agent_source(&raw).unwrap_err();
    assert!(
        err.contains(&format!("agent version {}", AGENT_FORMAT + 1)),
        "{err}"
    );
    assert!(!err.contains("unknown field"), "{err}");
}
