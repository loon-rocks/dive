use graph_config::pack::{
    load_lock, load_manifest, lock_migration_count, lock_to_string, pack_migration_count,
    parse_lock, parse_manifest, write_lock, LockEntry, PackLock, LOCK_FORMAT, LOCK_FORMAT_OLDEST,
    PACK_FORMAT, PACK_FORMAT_OLDEST,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixture_dir(kind: &str, version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(kind)
        .join(format!("v{version}"))
}

fn fixtures_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths
}

const HASH: &str = "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
const SHA: &str = "a8303d2c0f5e4d1b9a7c6e5f4d3c2b1a09876543";

fn manifest_error(text: &str) -> String {
    parse_manifest(text).unwrap_err()
}

fn lock_error(text: &str) -> String {
    parse_lock(text).unwrap_err()
}

#[test]
fn every_pack_and_lock_version_has_fixtures_that_still_load() {
    for version in PACK_FORMAT_OLDEST..=PACK_FORMAT {
        let fixtures = fixtures_in(&fixture_dir("pack", version));
        assert!(
            !fixtures.is_empty(),
            "pack version {version} has no fixtures"
        );
        for path in fixtures {
            let manifest = load_manifest(&path)
                .unwrap_or_else(|e| panic!("{} no longer loads: {e}", path.display()));
            assert_eq!(manifest.version, PACK_FORMAT, "{}", path.display());
            let declared: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
            assert_eq!(
                declared["version"].as_integer(),
                Some(i64::from(version)),
                "{} sits in v{version} but declares another version",
                path.display()
            );
        }
    }
    for version in LOCK_FORMAT_OLDEST..=LOCK_FORMAT {
        let fixtures = fixtures_in(&fixture_dir("lock", version));
        assert!(
            !fixtures.is_empty(),
            "lock version {version} has no fixtures"
        );
        for path in fixtures {
            let lock = load_lock(&path)
                .unwrap_or_else(|e| panic!("{} no longer loads: {e}", path.display()));
            assert_eq!(lock.version, LOCK_FORMAT, "{}", path.display());
        }
    }
}

#[test]
fn golden_pairs_load_identically_across_versions() {
    for version in (PACK_FORMAT_OLDEST..=PACK_FORMAT).filter(|v| *v < PACK_FORMAT) {
        for older in fixtures_in(&fixture_dir("pack", version)) {
            let newer = fixture_dir("pack", version + 1).join(older.file_name().unwrap());
            if newer.exists() {
                assert_eq!(load_manifest(&older), load_manifest(&newer));
            }
        }
    }
    for version in (LOCK_FORMAT_OLDEST..=LOCK_FORMAT).filter(|v| *v < LOCK_FORMAT) {
        for older in fixtures_in(&fixture_dir("lock", version)) {
            let newer = fixture_dir("lock", version + 1).join(older.file_name().unwrap());
            if newer.exists() {
                assert_eq!(load_lock(&older), load_lock(&newer));
            }
        }
    }
}

#[test]
fn migration_chains_match_the_current_versions() {
    assert_eq!(
        pack_migration_count() as u32,
        PACK_FORMAT - PACK_FORMAT_OLDEST
    );
    assert_eq!(
        lock_migration_count() as u32,
        LOCK_FORMAT - LOCK_FORMAT_OLDEST
    );
}

#[test]
fn the_everything_manifest_reads_every_field() {
    let manifest = load_manifest(&fixture_dir("pack", 1).join("everything.toml")).unwrap();
    assert_eq!(manifest.name, "example");
    assert_eq!(manifest.license.as_deref(), Some("MIT"));
    assert_eq!(manifest.requires.graph.as_deref(), Some(">=0.17"));
    assert_eq!(manifest.requires.os, ["macos", "linux"]);
    assert_eq!(manifest.requires.binaries, ["gh", "jq"]);
    assert_eq!(manifest.requires.env, ["GH_TOKEN"]);
    let linear = &manifest.requires.mcp["linear"];
    assert_eq!(linear.server.as_deref(), Some("linear-mcp-server"));
    assert_eq!(linear.version.as_deref(), Some(">=1.0"));
    assert_eq!(linear.url.as_deref(), Some("https://mcp.linear.app/mcp"));
    let minimal = load_manifest(&fixture_dir("pack", 1).join("minimal.toml")).unwrap();
    assert_eq!(minimal.license, None);
    assert!(minimal.requires.mcp.is_empty());
}

#[test]
fn a_newer_version_is_refused_by_name_before_the_schema_is_consulted() {
    let err = manifest_error(&format!(
        "version = {}\nname = \"x\"\nkey_from_the_future = 1\n",
        PACK_FORMAT + 1
    ));
    assert!(err.starts_with("pack.toml is pack version"), "{err}");
    assert!(err.contains("Upgrade graph"), "{err}");
    assert!(!err.contains("key_from_the_future"), "{err}");
    let err = lock_error(&format!(
        "version = {}\n[packs.x]\nkey_from_the_future = 1\n",
        LOCK_FORMAT + 1
    ));
    assert!(err.starts_with("packs.lock is lock version"), "{err}");
    assert!(!err.contains("key_from_the_future"), "{err}");
}

#[test]
fn the_version_is_required_and_must_be_a_positive_integer() {
    let err = manifest_error("name = \"x\"\ndescription = \"d\"\n");
    assert!(err.contains("version is required"), "{err}");
    let err = lock_error("");
    assert!(err.contains("version is required"), "{err}");
    let err = manifest_error("version = \"1\"\nname = \"x\"\ndescription = \"d\"\n");
    assert!(err.contains("positive integer"), "{err}");
}

#[test]
fn name_and_description_are_required() {
    let err = manifest_error("version = 1\nname = \"x\"\n");
    assert!(err.contains("description"), "{err}");
    let err = manifest_error("version = 1\ndescription = \"d\"\n");
    assert!(err.contains("name"), "{err}");
    let err = manifest_error("version = 1\nname = \"x\"\ndescription = \" \"\n");
    assert!(err.contains("description: must not be empty"), "{err}");
    let err = manifest_error("version = 1\nname = \"Bad\"\ndescription = \"d\"\n");
    assert!(err.contains("name: pack name 'Bad'"), "{err}");
}

#[test]
fn unknown_manifest_keys_are_refused_at_every_depth_by_their_path() {
    let base = "version = 1\nname = \"x\"\ndescription = \"d\"\n";
    for (extra, key) in [
        ("authors = []\n", "`authors`"),
        ("[requires]\npython = \">=3\"\n", "`requires.python`"),
        (
            "[requires.mcp.linear]\nserver = \"s\"\ntransport = \"http\"\n",
            "`requires.mcp.linear.transport`",
        ),
    ] {
        let err = manifest_error(&format!("{base}{extra}"));
        assert!(err.contains("unknown key"), "{err}");
        assert!(err.contains(key), "{key}: {err}");
    }
}

#[test]
fn requirements_are_validated_naming_the_field() {
    let base = "version = 1\nname = \"x\"\ndescription = \"d\"\n";
    let err = manifest_error(&format!("{base}[requires]\ngraph = \"at least 0.17\"\n"));
    assert!(err.contains("requires.graph: 'at least 0.17'"), "{err}");
    let err = manifest_error(&format!(
        "{base}[requires]\nos = [\"macos\", \"windows\"]\n"
    ));
    assert!(err.contains("requires.os: 'windows'"), "{err}");
    let err = manifest_error(&format!(
        "{base}[requires.mcp.linear]\nversion = \"one point oh\"\n"
    ));
    assert!(err.contains("requires.mcp.linear.version"), "{err}");
    let ok = parse_manifest(&format!(
        "{base}[requires]\ngraph = \"^0.17.1, <1\"\n[requires.mcp.linear]\nversion = \"~1.2\"\n"
    ));
    assert!(ok.is_ok(), "{ok:?}");
}

#[test]
fn lock_entries_are_validated_naming_the_field() {
    let entry = |sha: &str, hash: &str| {
        format!("version = 1\n[packs.example]\ngit = \"g\"\nsha = \"{sha}\"\nhash = \"{hash}\"\n")
    };
    assert!(parse_lock(&entry(SHA, HASH)).is_ok());
    let err = lock_error(&entry(SHA, "md5:abc"));
    assert!(err.contains("packs.example.hash"), "{err}");
    let err = lock_error(&entry(
        SHA,
        &HASH.to_uppercase().replace("SHA256", "sha256"),
    ));
    assert!(err.contains("packs.example.hash"), "{err}");
    let err = lock_error(&entry("a8303d2", HASH));
    assert!(err.contains("packs.example.sha"), "{err}");
    let err = lock_error(&format!(
        "version = 1\n[packs.example]\ngit = \"g\"\nsha = \"{SHA}\"\nhash = \"{HASH}\"\nref = \"v1\"\n"
    ));
    assert!(err.contains("`packs.example.ref`"), "{err}");
    let err = lock_error(&format!(
        "version = 1\n[packs.Example]\ngit = \"g\"\nsha = \"{SHA}\"\nhash = \"{HASH}\"\n"
    ));
    assert!(err.contains("packs.Example"), "{err}");
    let err = lock_error("version = 1\n[packs.example]\ngit = \"g\"\n");
    assert!(err.contains("sha"), "{err}");
}

#[test]
fn a_missing_lock_is_an_empty_one() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        load_lock(&dir.path().join("packs.lock")).unwrap(),
        PackLock::default()
    );
}

#[test]
fn the_lock_writer_is_sorted_stable_and_round_trips() {
    let mut packs = BTreeMap::new();
    packs.insert(
        "zeta".to_string(),
        LockEntry {
            git: "github.com/acme/graph-pack-zeta".into(),
            tag: None,
            sha: SHA.into(),
            hash: HASH.into(),
        },
    );
    packs.insert(
        "alpha".to_string(),
        LockEntry {
            git: "github.com/acme/graph-pack-alpha".into(),
            tag: Some("v1.2.0".into()),
            sha: SHA.into(),
            hash: HASH.into(),
        },
    );
    let lock = PackLock {
        version: LOCK_FORMAT,
        packs,
    };
    let text = lock_to_string(&lock);
    assert_eq!(
        text,
        format!(
            "version = {LOCK_FORMAT}\n\n[packs.alpha]\ngit = \"github.com/acme/graph-pack-alpha\"\ntag = \"v1.2.0\"\nsha = \"{SHA}\"\nhash = \"{HASH}\"\n\n[packs.zeta]\ngit = \"github.com/acme/graph-pack-zeta\"\nsha = \"{SHA}\"\nhash = \"{HASH}\"\n"
        )
    );
    assert_eq!(parse_lock(&text).unwrap(), lock);
    assert_eq!(lock_to_string(&parse_lock(&text).unwrap()), text);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packs.lock");
    write_lock(&path, &lock).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert_eq!(load_lock(&path).unwrap(), lock);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    let fixture = load_lock(&fixture_dir("lock", 1).join("everything.toml")).unwrap();
    assert_eq!(parse_lock(&lock_to_string(&fixture)).unwrap(), fixture);
    assert_eq!(
        lock_to_string(&PackLock::default()),
        format!("version = {LOCK_FORMAT}\n")
    );
}
