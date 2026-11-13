use anyhow::Result;
pub use dive_runtime::dev_key::{
    refuse_in_production, DevKey, DEV_AUDIENCE, DEV_ISSUER, TOKEN_LIFETIME_SECS,
};
use serde_json::json;
use std::path::PathBuf;

pub struct TokenRequest<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub plan: Option<&'a str>,
    pub tenant: &'a str,
    pub sub: &'a str,
    pub scope: &'a str,
    pub now: i64,
}

pub fn claims(request: &TokenRequest) -> serde_json::Value {
    let mut claims = json!({
        "iss": request.issuer,
        "aud": request.audience,
        "sub": request.sub,
        "tenant": request.tenant,
        "scope": request.scope,
        "iat": request.now,
        "exp": request.now + TOKEN_LIFETIME_SECS,
        "jti": uuid::Uuid::new_v4().to_string(),
    });
    if let Some(plan) = request.plan {
        claims["plan"] = json!(plan);
    }
    claims
}

pub fn print_token(
    dir: Option<PathBuf>,
    plan: Option<String>,
    tenant: String,
    sub: String,
    scope: String,
) -> Result<()> {
    refuse_in_production(std::env::var("DIVE_ENV").ok().as_deref())?;
    let workspace = super::workspace(dir.as_deref())?;
    let loaded = dive_runtime::runtime::load_serve_config(&workspace)?;
    refuse_in_production(loaded.config.identity.environment.as_deref())?;
    let data_dir = dive_config::expand_tilde(&loaded.config.settings.data_dir);
    let key = DevKey::load(&data_dir)?;
    let serve = loaded.config.serve.unwrap_or_default();
    let token = key.sign(&claims(&TokenRequest {
        issuer: serve.issuer.as_deref().unwrap_or(DEV_ISSUER),
        audience: serve.audience.as_deref().unwrap_or(DEV_AUDIENCE),
        plan: plan.as_deref(),
        tenant: &tenant,
        sub: &sub,
        scope: &scope,
        now: chrono::Utc::now().timestamp(),
    }))?;
    crate::commands::outcome::report(
        crate::commands::outcome::Outcome::raw(format!("{token}\n"), json!({ "token": token })),
        false,
    )
}

pub fn print_public_key(dir: Option<PathBuf>) -> Result<()> {
    refuse_in_production(std::env::var("DIVE_ENV").ok().as_deref())?;
    let workspace = super::workspace(dir.as_deref())?;
    let loaded = dive_runtime::runtime::load_config(&workspace)?;
    refuse_in_production(loaded.config.identity.environment.as_deref())?;
    let data_dir = dive_config::expand_tilde(&loaded.config.settings.data_dir);
    let key = DevKey::load_or_create(&data_dir)?;
    let path = DevKey::public_path(&data_dir);
    crate::commands::outcome::report(
        crate::commands::outcome::Outcome::raw(
            key.public_pem().to_string(),
            json!({ "public_key": key.public_pem(), "path": path }),
        ),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_signed_with_the_development_key_verifies_against_its_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = DevKey::load_or_create(dir.path()).unwrap();
        let token = key
            .sign(&claims(&TokenRequest {
                issuer: DEV_ISSUER,
                audience: DEV_AUDIENCE,
                plan: Some("triage"),
                tenant: "acme",
                sub: "ada",
                scope: "runs:read",
                now: chrono::Utc::now().timestamp(),
            }))
            .unwrap();
        let verifier = crate::serve::auth::Verifier::new(DEV_ISSUER, DEV_AUDIENCE)
            .trust_pem(key.public_pem().as_bytes(), "dev")
            .unwrap();
        let claims = verifier.verify(&token).unwrap();
        assert_eq!(claims.plan.as_deref(), Some("triage"));
        assert!(claims.has_scope("runs:read"));
        assert_eq!(claims.jti.map(|jti| jti.len()), Some(36));
    }
}
