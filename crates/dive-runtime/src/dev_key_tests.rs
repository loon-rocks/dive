use super::*;
use dive_config::DevIdentity;

fn ada() -> DevIdentity {
    DevIdentity {
        tenant: "acme".to_string(),
        user: "ada@example.com".to_string(),
        claims: [("role".to_string(), serde_json::json!("admin"))].into(),
    }
}

fn decode(token: &str, key: &DevKey) -> serde_json::Value {
    let decoding = jsonwebtoken::DecodingKey::from_ed_pem(key.public_pem().as_bytes()).unwrap();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
    validation.set_audience(&[DEV_AUDIENCE]);
    validation.set_issuer(&[DEV_ISSUER]);
    jsonwebtoken::decode::<serde_json::Value>(token, &decoding, &validation)
        .unwrap()
        .claims
}

#[test]
fn the_key_is_the_same_on_every_load() {
    let dir = tempfile::tempdir().unwrap();
    let key = DevKey::load_or_create(dir.path()).unwrap();
    let again = DevKey::load_or_create(dir.path()).unwrap();
    assert_eq!(key.public_pem(), again.public_pem());
}

#[test]
fn the_public_key_is_written_beside_the_private_one() {
    let dir = tempfile::tempdir().unwrap();
    let key = DevKey::load_or_create(dir.path()).unwrap();
    let written = std::fs::read_to_string(DevKey::public_path(dir.path())).unwrap();
    assert_eq!(written, key.public_pem());
}

#[test]
fn a_minted_token_verifies_against_the_public_key() {
    let dir = tempfile::tempdir().unwrap();
    let minter = DevMinter::new(DevKey::load_or_create(dir.path()).unwrap(), None);
    let token = minter.mint(&ada(), "run-1", Some("triage")).unwrap();
    let key = DevKey::load(dir.path()).unwrap();
    let claims = decode(token.expose(), &key);
    assert_eq!(
        (
            &claims["sub"],
            &claims["tenant"],
            &claims["jti"],
            &claims["plan"]
        ),
        (
            &serde_json::json!("ada@example.com"),
            &serde_json::json!("acme"),
            &serde_json::json!("run-1"),
            &serde_json::json!("triage")
        )
    );
}

#[test]
fn a_minted_token_carries_the_identitys_extra_claims() {
    let dir = tempfile::tempdir().unwrap();
    let minter = DevMinter::new(DevKey::load_or_create(dir.path()).unwrap(), None);
    let claims = minter.claims(&ada(), "run-1", None, 1_000);
    assert_eq!(claims["role"], serde_json::json!("admin"));
}

#[test]
fn a_minted_token_marks_itself_as_a_dev_token() {
    let dir = tempfile::tempdir().unwrap();
    let minter = DevMinter::new(DevKey::load_or_create(dir.path()).unwrap(), None);
    let claims = minter.claims(&ada(), "run-1", None, 1_000);
    assert_eq!(
        (&claims["iss"], &claims["dive_dev"], &claims["exp"]),
        (
            &serde_json::json!(DEV_ISSUER),
            &serde_json::json!(true),
            &serde_json::json!(1_000 + TOKEN_LIFETIME_SECS)
        )
    );
}

#[test]
fn the_audience_follows_the_serve_audience_when_set() {
    let dir = tempfile::tempdir().unwrap();
    let minter = DevMinter::new(DevKey::load_or_create(dir.path()).unwrap(), Some("app"));
    assert_eq!(
        minter.claims(&ada(), "r", None, 0)["aud"],
        serde_json::json!("app")
    );
}

#[test]
fn the_development_key_is_refused_in_production() {
    assert!(refuse_in_production(Some("Production")).is_err());
}

#[test]
fn the_development_key_is_allowed_outside_production() {
    assert!(refuse_in_production(Some("staging")).is_ok() && refuse_in_production(None).is_ok());
}
