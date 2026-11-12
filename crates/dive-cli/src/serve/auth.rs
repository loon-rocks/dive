use super::error::ServeError;
use super::AppState;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

pub const LEEWAY_SECS: u64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Ed,
    Ec,
    Rsa,
}

impl Family {
    fn of(algorithm: Algorithm) -> Option<Self> {
        match algorithm {
            Algorithm::EdDSA => Some(Self::Ed),
            Algorithm::ES256 => Some(Self::Ec),
            Algorithm::RS256 => Some(Self::Rsa),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct Key {
    family: Family,
    key: DecodingKey,
}

#[derive(Clone)]
pub struct Verifier {
    keys: Vec<Key>,
    issuer: String,
    audience: String,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier")
            .field("keys", &self.keys.len())
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish()
    }
}

fn parse_key(pem: &[u8]) -> Option<Key> {
    if let Ok(key) = DecodingKey::from_ed_pem(pem) {
        return Some(Key {
            family: Family::Ed,
            key,
        });
    }
    if let Ok(key) = DecodingKey::from_ec_pem(pem) {
        return Some(Key {
            family: Family::Ec,
            key,
        });
    }
    DecodingKey::from_rsa_pem(pem).ok().map(|key| Key {
        family: Family::Rsa,
        key,
    })
}

impl Verifier {
    pub fn new(issuer: &str, audience: &str) -> Self {
        Self {
            keys: Vec::new(),
            issuer: issuer.to_string(),
            audience: audience.to_string(),
        }
    }

    pub fn trust_pem(mut self, pem: &[u8], label: &str) -> anyhow::Result<Self> {
        let key = parse_key(pem).ok_or_else(|| {
            anyhow::anyhow!("{label} isn't an Ed25519, P-256 or RSA public key in PEM")
        })?;
        self.keys.push(key);
        Ok(self)
    }

    pub fn trust_file(self, path: &Path) -> anyhow::Result<Self> {
        let pem = std::fs::read(path)
            .map_err(|error| anyhow::anyhow!("can't read {}: {error}", path.display()))?;
        self.trust_pem(&pem, &path.display().to_string())
    }

    pub fn verify(&self, token: &str) -> Result<Claims, ServeError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| ServeError::InvalidToken("the token isn't a JWT".into()))?;
        let family = Family::of(header.alg).ok_or_else(|| {
            ServeError::InvalidToken(format!(
                "tokens signed with {:?} aren't accepted; use EdDSA, ES256 or RS256",
                header.alg
            ))
        })?;
        let mut validation = Validation::new(header.alg);
        validation.leeway = LEEWAY_SECS;
        validation.validate_nbf = true;
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let mut last = ServeError::InvalidToken("no key can verify this token".into());
        for key in self.keys.iter().filter(|key| key.family == family) {
            match jsonwebtoken::decode::<Claims>(token, &key.key, &validation) {
                Ok(data) => return data.claims.checked(),
                Err(error) => {
                    last = match error.kind() {
                        jsonwebtoken::errors::ErrorKind::InvalidSignature => last,
                        jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                            ServeError::InvalidToken("the token has expired".into())
                        }
                        jsonwebtoken::errors::ErrorKind::InvalidIssuer => {
                            ServeError::InvalidToken("the token's issuer isn't trusted".into())
                        }
                        jsonwebtoken::errors::ErrorKind::InvalidAudience => {
                            ServeError::InvalidToken("the token isn't meant for this server".into())
                        }
                        jsonwebtoken::errors::ErrorKind::ImmatureSignature => {
                            ServeError::InvalidToken("the token isn't valid yet".into())
                        }
                        _ => ServeError::InvalidToken("the token is malformed".into()),
                    };
                }
            }
        }
        Err(last)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub tenant: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub jti: Option<String>,
}

impl Claims {
    fn checked(self) -> Result<Self, ServeError> {
        if self.sub.trim().is_empty() {
            return Err(ServeError::InvalidToken("the token has no sub".into()));
        }
        if self.tenant.trim().is_empty() {
            return Err(ServeError::InvalidToken("the token has no tenant".into()));
        }
        Ok(self)
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope
            .split_whitespace()
            .any(|granted| granted == scope)
    }

    pub fn require(&self, scope: &str) -> Result<(), ServeError> {
        if self.has_scope(scope) {
            Ok(())
        } else {
            Err(ServeError::Forbidden(format!(
                "the token lacks the {scope} scope"
            )))
        }
    }
}

pub struct Caller {
    pub claims: Claims,
    pub token: String,
}

impl FromRequestParts<Arc<AppState>> for Caller {
    type Rejection = ServeError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .ok_or_else(|| ServeError::InvalidToken("missing Authorization header".into()))?
            .to_str()
            .map_err(|_| ServeError::InvalidToken("unreadable Authorization header".into()))?;
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ServeError::InvalidToken("expected a Bearer token".into()))?;
        let claims = state.verifier.verify(token)?;
        Ok(Self {
            claims,
            token: token.to_string(),
        })
    }
}
