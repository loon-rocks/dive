use super::*;

fn chain(env: MemorySecrets, store: MemorySecrets) -> ChainedSource {
    ChainedSource::new(vec![Arc::new(env), Arc::new(store)])
}

#[test]
fn the_environment_wins_the_store_fills_a_miss_and_both_missing_is_none() {
    let source = chain(
        MemorySecrets::env().with("BOTH", "from-env"),
        MemorySecrets::store()
            .with("BOTH", "from-store")
            .with("STORED", "only-stored"),
    );
    let both = source.resolve("BOTH").unwrap();
    assert_eq!(
        (both.value.as_str(), both.source),
        ("from-env", Origin::Env)
    );
    let stored = source.resolve("STORED").unwrap();
    assert_eq!(
        (stored.value.as_str(), stored.source),
        ("only-stored", Origin::Store)
    );
    assert_eq!(source.resolve("NEITHER"), None);
}

#[test]
fn an_empty_environment_value_never_shadows_a_stored_one_but_still_counts_when_nothing_is_stored() {
    let source = chain(
        MemorySecrets::env().with("EMPTY", "").with("BLANK", ""),
        MemorySecrets::store().with("EMPTY", "stored"),
    );
    assert_eq!(source.resolve("EMPTY").unwrap().value, "stored");
    let blank = source.resolve("BLANK").unwrap();
    assert_eq!((blank.value.as_str(), blank.source), ("", Origin::Env));
}

#[test]
fn an_unavailable_store_is_a_miss_not_an_error() {
    let source = chain(
        MemorySecrets::env().with("SET", "value"),
        MemorySecrets::unavailable(),
    );
    assert_eq!(source.resolve("SET").unwrap().value, "value");
    assert_eq!(source.resolve("UNSET"), None);
    assert!(matches!(
        MemorySecrets::unavailable().set("X", "y"),
        Err(StoreError::Unavailable(_))
    ));
}

#[test]
fn a_disabled_credential_store_is_unavailable_and_resolves_nothing() {
    let store = CredentialStore::disabled();
    assert!(matches!(store.status(), Err(StoreError::Unavailable(_))));
    assert!(matches!(
        store.get("DIVE_TEST_NEVER_STORED"),
        Err(StoreError::Unavailable(_))
    ));
    assert_eq!(store.resolve("DIVE_TEST_NEVER_STORED"), None);
    assert!(matches!(
        store.set("not a name", "x"),
        Err(StoreError::InvalidName(_))
    ));
}

#[test]
fn stores_refuse_names_that_are_not_variable_names() {
    let store = MemorySecrets::store();
    assert!(matches!(
        store.set("FOO:-bar", "x"),
        Err(StoreError::InvalidName(_))
    ));
    assert_eq!(store.resolve("FOO:-bar"), None);
}

#[test]
fn removing_reports_whether_anything_was_stored() {
    let store = MemorySecrets::store().with("KEY", "value");
    assert!(store.remove("KEY").unwrap());
    assert!(!store.remove("KEY").unwrap());
    assert_eq!(store.get("KEY").unwrap(), None);
}

#[test]
fn debug_output_names_secrets_without_their_values() {
    let store = MemorySecrets::store().with("KEY", "sk-very-secret");
    let debug = format!("{store:?} {:?}", store.resolve("KEY"));
    assert!(debug.contains("KEY"));
    assert!(!debug.contains("sk-very-secret"));
}

#[test]
fn env_references_read_names_and_defaults() {
    assert_eq!(
        env_references("Bearer ${TOKEN} at ${URL:-https://x} ${OPT:-} ${broken"),
        vec![
            EnvReference {
                name: "TOKEN",
                default: None
            },
            EnvReference {
                name: "URL",
                default: Some("https://x")
            },
            EnvReference {
                name: "OPT",
                default: Some("")
            },
        ]
    );
}
