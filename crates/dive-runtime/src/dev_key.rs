use anyhow::{bail, Context, Result};
use base64::Engine;
use jsonwebtoken::{EncodingKey, Header};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::path::{Path, PathBuf};

pub const DEV_ISSUER: &str = "dive-dev";
pub const DEV_AUDIENCE: &str = "dive";
pub const TOKEN_LIFETIME_SECS: i64 = 3600;
const KEY_FILE: &str = "serve/dev-key.pem";
const PUBLIC_KEY_FILE: &str = "serve/dev-key.pub.pem";
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

    pub fn public_path(data_dir: &Path) -> PathBuf {
        data_dir.join(PUBLIC_KEY_FILE)
    }

    fn publish(&self, data_dir: &Path) -> Result<()> {
        let path = Self::public_path(data_dir);
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() == Some(self.public_pem.as_str()) {
            return Ok(());
        }
        std::fs::write(&path, &self.public_pem)
            .with_context(|| format!("can't write {}", path.display()))
    }

    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("can't read {}", path.display()))?;
            let key = Self::from_pkcs8(&unpem(&text)?)?;
            key.publish(data_dir)?;
            return Ok(key);
        }
        let key = Self::generate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("can't create {}", parent.display()))?;
        }
        write_private(&path, key.private_pem.as_bytes())?;
        key.publish(data_dir)?;
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
    if is_production(env) {
        bail!("the development key is refused in production (DIVE_ENV or [identity].environment)");
    }
    Ok(())
}

pub fn is_production(environment: Option<&str>) -> bool {
    environment.is_some_and(|env| env.eq_ignore_ascii_case("production"))
}

pub struct DevMinter {
    key: DevKey,
    audience: String,
}

impl DevMinter {
    pub fn new(key: DevKey, audience: Option<&str>) -> Self {
        Self {
            key,
            audience: audience.unwrap_or(DEV_AUDIENCE).to_string(),
        }
    }

    pub fn claims(
        &self,
        identity: &dive_config::DevIdentity,
        run_id: &str,
        plan: Option<&str>,
        now: i64,
    ) -> serde_json::Value {
        let mut claims: serde_json::Map<String, serde_json::Value> = identity
            .claims
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        for (name, value) in [
            ("iss", serde_json::json!(DEV_ISSUER)),
            ("aud", serde_json::json!(self.audience)),
            ("sub", serde_json::json!(identity.user)),
            ("tenant", serde_json::json!(identity.tenant)),
            ("jti", serde_json::json!(run_id)),
            ("iat", serde_json::json!(now)),
            ("exp", serde_json::json!(now + TOKEN_LIFETIME_SECS)),
            ("dive_dev", serde_json::json!(true)),
        ] {
            claims.insert(name.to_string(), value);
        }
        if let Some(plan) = plan {
            claims.insert("plan".to_string(), serde_json::json!(plan));
        }
        serde_json::Value::Object(claims)
    }

    pub fn mint(
        &self,
        identity: &dive_config::DevIdentity,
        run_id: &str,
        plan: Option<&str>,
    ) -> Result<dive_core::run::RunToken> {
        let claims = self.claims(identity, run_id, plan, chrono::Utc::now().timestamp());
        Ok(dive_core::run::RunToken::new(self.key.sign(&claims)?))
    }
}

#[cfg(test)]
#[path = "dev_key_tests.rs"]
mod tests;
