use anyhow::{bail, Context, Result};
use base64::Engine;
use jsonwebtoken::{EncodingKey, Header};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const DEV_ISSUER: &str = "dive-dev";
pub const DEV_AUDIENCE: &str = "dive";
pub const TOKEN_LIFETIME_SECS: i64 = 3600;
const KEY_FILE: &str = "serve/dev-key.pem";
const SPKI_ED25519_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

pub struct DevKey {
    private_pem: String,
    public_pem: String,
}

fn pem(label: &str, der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = encoded
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

fn unpem(text: &str) -> Result<Vec<u8>> {
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .context("the development key isn't valid PEM")
}

impl DevKey {
    fn from_pkcs8(der: &[u8]) -> Result<Self> {
        let pair = Ed25519KeyPair::from_pkcs8(der)
            .map_err(|_| anyhow::anyhow!("the development key isn't an Ed25519 key"))?;
        let mut spki = SPKI_ED25519_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        Ok(Self {
            private_pem: pem("PRIVATE KEY", der),
            public_pem: pem("PUBLIC KEY", &spki),
        })
    }

    pub fn generate() -> Result<Self> {
        let der = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("couldn't generate a development key"))?;
        Self::from_pkcs8(der.as_ref())
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(KEY_FILE)
    }

    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("can't read {}", path.display()))?;
            return Self::from_pkcs8(&unpem(&text)?);
        }
        let key = Self::generate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("can't create {}", parent.display()))?;
        }
        write_private(&path, key.private_pem.as_bytes())?;
        Ok(key)
    }

    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        if !path.exists() {
            bail!(
                "no development key at {}; run `dive serve --dev-key` once to create it",
                path.display()
            );
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("can't read {}", path.display()))?;
        Self::from_pkcs8(&unpem(&text)?)
    }

    pub fn public_pem(&self) -> &str {
        &self.public_pem
    }

    pub fn sign(&self, claims: &serde_json::Value) -> Result<String> {
        let key = EncodingKey::from_ed_pem(self.private_pem.as_bytes())
            .context("the development key can't sign")?;
        jsonwebtoken::encode(&Header::new(jsonwebtoken::Algorithm::EdDSA), claims, &key)
            .context("couldn't sign the token")
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("can't create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("can't write {}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("can't write {}", path.display()))
}

pub fn refuse_in_production(env: Option<&str>) -> Result<()> {
    if env.is_some_and(|env| env.eq_ignore_ascii_case("production")) {
        bail!("the development key is refused in production (DIVE_ENV or [identity].environment)");
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_signed_with_the_development_key_verifies_against_its_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = DevKey::load_or_create(dir.path()).unwrap();
        let again = DevKey::load_or_create(dir.path()).unwrap();
        assert_eq!(key.public_pem(), again.public_pem());
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

    #[test]
    fn the_development_key_is_refused_in_production() {
        assert!(refuse_in_production(Some("production")).is_err());
        assert!(refuse_in_production(Some("staging")).is_ok());
        assert!(refuse_in_production(None).is_ok());
    }
}
