use crate::packs::InstalledPacks;
use crate::runtime::{Mode, Runtime, RuntimeOptions};
use dive_config::{Config, DevIdentity, ModelRoles};
use dive_llm::ModelRouter;
use std::collections::HashMap;
use std::sync::Arc;

fn config(data_dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.tools.paths = Vec::new();
    config.plans.paths = Vec::new();
    config.settings.data_dir = data_dir.to_path_buf();
    config.identities.insert(
        "ada".to_string(),
        DevIdentity {
            tenant: "acme".to_string(),
            user: "ada@example.com".to_string(),
            claims: [("role".to_string(), serde_json::json!("admin"))].into(),
        },
    );
    config
}

fn runtime(config: Config, options: RuntimeOptions) -> Runtime {
    let options = RuntimeOptions {
        storage_backend: Some("memory".to_string()),
        ..options
    };
    Runtime::with_router(
        config,
        InstalledPacks::default(),
        options,
        ModelRouter::with_providers(HashMap::new(), ModelRoles::default()),
    )
    .expect("the test runtime opens")
}

fn acting_as(name: &str) -> RuntimeOptions {
    RuntimeOptions {
        acting_as: Some(name.to_string()),
        ..Default::default()
    }
}

fn claims(token: &str, data_dir: &std::path::Path) -> serde_json::Value {
    let key = crate::dev_key::DevKey::load(data_dir).unwrap();
    let decoding = jsonwebtoken::DecodingKey::from_ed_pem(key.public_pem().as_bytes()).unwrap();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
    validation.set_audience(&["dive"]);
    jsonwebtoken::decode::<serde_json::Value>(token, &decoding, &validation)
        .unwrap()
        .claims
}

#[test]
fn a_run_acting_as_an_identity_carries_a_token_minted_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(config(dir.path()), acting_as("ada"));
    let run = runtime.plan_run(Arc::new(dive_core::NullSink), "triage");
    let claims = claims(run.token.as_ref().unwrap().expose(), dir.path());
    assert_eq!(
        (
            &claims["sub"],
            &claims["tenant"],
            &claims["plan"],
            &claims["jti"]
        ),
        (
            &serde_json::json!("ada@example.com"),
            &serde_json::json!("acme"),
            &serde_json::json!("triage"),
            &serde_json::json!(run.id.as_str())
        )
    );
}

#[test]
fn a_run_acting_as_an_identity_is_stamped_with_its_tenant_and_user() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(config(dir.path()), acting_as("ada"));
    let run = runtime.run(Arc::new(dive_core::NullSink));
    assert_eq!(
        (run.identity.user.as_str(), run.identity.tenant.as_str()),
        ("ada@example.com", "acme")
    );
}

#[test]
fn each_run_gets_its_own_token() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(config(dir.path()), acting_as("ada"));
    let first = runtime.run(Arc::new(dive_core::NullSink));
    let second = runtime.run(Arc::new(dive_core::NullSink));
    assert_ne!(
        first.token.as_ref().unwrap().expose(),
        second.token.as_ref().unwrap().expose()
    );
}

#[test]
fn the_default_identity_applies_without_as() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.dev.identity = Some("ada".to_string());
    let runtime = runtime(config, RuntimeOptions::default());
    assert!(runtime.run(Arc::new(dive_core::NullSink)).token.is_some());
}

#[test]
fn without_an_identity_runs_carry_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(config(dir.path()), RuntimeOptions::default());
    assert!(runtime.run(Arc::new(dive_core::NullSink)).token.is_none());
}

#[test]
fn acting_as_an_unknown_identity_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(config(dir.path()), acting_as("ghost"));
    let error = runtime.origin_as(None).err().unwrap();
    assert!(
        format!("{error:#}").contains("no dev identity 'ghost'"),
        "{error:#}"
    );
}

#[test]
fn acting_as_an_identity_is_refused_in_production() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.identity.environment = Some("production".to_string());
    let runtime = runtime(config, acting_as("ada"));
    assert!(runtime.origin_as(None).is_err());
}

#[test]
fn the_default_identity_mints_nothing_in_production() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.identity.environment = Some("production".to_string());
    config.dev.identity = Some("ada".to_string());
    let runtime = runtime(config, RuntimeOptions::default());
    assert!(runtime.run(Arc::new(dive_core::NullSink)).token.is_none());
}

#[test]
fn serve_mode_never_mints() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        config(dir.path()),
        RuntimeOptions {
            mode: Mode::Serve,
            ..acting_as("ada")
        },
    );
    assert!(runtime.origin_as(None).unwrap().acting_as().is_none());
}
