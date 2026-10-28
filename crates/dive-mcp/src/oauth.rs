use anyhow::{anyhow, bail, Context, Result};
use dive_config::McpOAuth;
use dive_core::secrets::{SignInStore, StoreError};
use rmcp::service::{ClientInitializeError, ServiceError};
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationSession, CredentialStore,
    OAuthClientConfig, StoredCredentials,
};
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::transport::DynamicTransportError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

const CLIENT_NAME: &str = "Dive";

pub const APP_CALLBACK: &str = "http://127.0.0.1/callback";

#[derive(Debug, thiserror::Error)]
#[error("{server} doesn't let apps register themselves: create an OAuth app with the callback URL {APP_CALLBACK}, then add its client_id and client_secret under [mcp.{server}.oauth]")]
pub struct NeedsApp {
    pub server: String,
}

fn app_config(app: &McpOAuth, redirect: &str) -> OAuthClientConfig {
    let config = OAuthClientConfig::new(app.client_id.clone(), redirect.to_string())
        .with_scopes(app.scopes.clone());
    match &app.client_secret {
        Some(secret) => config.with_client_secret(secret.clone()),
        None => config,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInScope {
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignIn {
    pub scope: SignInScope,
    pub signed_in_at: u64,
    pub access: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    signed_in_at: u64,
    credentials: StoredCredentials,
}

impl Saved {
    fn sign_in(self, scope: SignInScope) -> SignIn {
        SignIn {
            scope,
            signed_in_at: self.signed_in_at,
            access: self.credentials.granted_scopes,
        }
    }
}

#[derive(Clone)]
pub struct SignIns {
    store: Arc<dyn SignInStore>,
    project: Option<String>,
}

impl SignIns {
    pub fn new(store: Arc<dyn SignInStore>, project: Option<String>) -> Self {
        Self { store, project }
    }

    pub fn for_project(&self, project: Option<String>) -> Self {
        Self {
            store: self.store.clone(),
            project,
        }
    }

    pub fn status(&self, url: &str) -> Result<Option<SignIn>, StoreError> {
        Ok(self.in_use(url)?.map(|(scope, saved)| saved.sign_in(scope)))
    }

    pub fn sign_out(&self, url: &str, scope: SignInScope) -> Result<bool, StoreError> {
        self.store.remove(url, self.target(scope)?)
    }

    pub async fn sign_in(
        &self,
        server: &str,
        url: &str,
        app: Option<&McpOAuth>,
        scope: SignInScope,
        open: impl FnOnce(&str),
        cancelled: impl Future<Output = ()>,
    ) -> Result<SignIn> {
        self.store.available()?;
        let project = self.target(scope)?.map(str::to_string);
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("couldn't open a local port for the browser to return to")?;
        let redirect = format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        );

        let failed = |error: AuthError| anyhow!("couldn't sign in to {server}: {error}");
        let mut manager = AuthorizationManager::new(url).await.map_err(failed)?;
        manager.set_credential_store(self.stored(url, project.clone(), true));
        let metadata = manager.discover_metadata().await.map_err(failed)?;
        let registers = metadata.registration_endpoint.is_some();
        manager.set_metadata(metadata);
        let scopes = match app {
            Some(app) if !app.scopes.is_empty() => app.scopes.clone(),
            _ => manager.select_scopes(None, &[]),
        };
        let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let session = match app {
            Some(app) => {
                manager
                    .configure_client(app_config(app, &redirect))
                    .map_err(failed)?;
                let address = manager
                    .get_authorization_url(&scopes)
                    .await
                    .map_err(failed)?;
                AuthorizationSession::for_scope_upgrade(manager, address, &redirect)
            }
            None if !registers => {
                return Err(NeedsApp {
                    server: server.to_string(),
                }
                .into())
            }
            None => AuthorizationSession::new(manager, &scopes, &redirect, Some(CLIENT_NAME), None)
                .await
                .map_err(failed)?,
        };
        open(session.get_authorization_url());

        let (stream, answer) = tokio::select! {
            waited = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_callback(&listener)) => {
                waited.map_err(|_| anyhow!("no answer from the browser within 5 minutes"))??
            }
            _ = cancelled => bail!("sign-in cancelled"),
        };
        let callback = match answer {
            Ok(callback) => callback,
            Err(refused) => {
                respond(
                    stream,
                    "200 OK",
                    &format!("{server} declined the sign-in"),
                    &refused,
                )
                .await;
                bail!("{server} declined the sign-in: {refused}");
            }
        };
        if let Err(error) = session
            .handle_callback_with_issuer(
                &callback.code,
                &callback.state,
                callback.issuer.as_deref(),
            )
            .await
        {
            respond(
                stream,
                "200 OK",
                &format!("Couldn't sign in to {server}"),
                &error.to_string(),
            )
            .await;
            return Err(failed(error));
        }
        respond(
            stream,
            "200 OK",
            &format!("Signed in to {server}"),
            "Dive has it from here. You can close this tab.",
        )
        .await;
        Ok(self
            .read(url, project.as_deref())?
            .context("the sign-in finished but nothing was saved")?
            .sign_in(scope))
    }

    pub(crate) async fn client(
        &self,
        url: &str,
        app: Option<&McpOAuth>,
    ) -> Result<Option<AuthClient<reqwest::Client>>, AuthError> {
        let project = match self.in_use(url) {
            Ok(Some((SignInScope::Project, _))) => self.project.clone(),
            Ok(Some((SignInScope::Global, _))) => None,
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::debug!(url, error = %error, "no sign-in: the credential store is unavailable");
                return Ok(None);
            }
        };
        let mut manager = AuthorizationManager::new(url).await?;
        manager.set_credential_store(self.stored(url, project, false));
        if !manager.initialize_from_store().await? {
            return Ok(None);
        }
        if let Some(app) = app {
            manager.configure_client(app_config(app, APP_CALLBACK))?;
        }
        Ok(Some(AuthClient::new(reqwest::Client::default(), manager)))
    }

    fn in_use(&self, url: &str) -> Result<Option<(SignInScope, Saved)>, StoreError> {
        if let Some(project) = self.project.as_deref() {
            if let Some(saved) = self.read(url, Some(project))? {
                return Ok(Some((SignInScope::Project, saved)));
            }
        }
        Ok(self
            .read(url, None)?
            .map(|saved| (SignInScope::Global, saved)))
    }

    fn stored(&self, url: &str, project: Option<String>, fresh: bool) -> Stored {
        Stored {
            store: self.store.clone(),
            url: url.to_string(),
            project,
            fresh,
        }
    }

    fn read(&self, url: &str, project: Option<&str>) -> Result<Option<Saved>, StoreError> {
        read(self.store.as_ref(), url, project)
    }

    fn target(&self, scope: SignInScope) -> Result<Option<&str>, StoreError> {
        match scope {
            SignInScope::Global => Ok(None),
            SignInScope::Project => self.project.as_deref().map(Some).ok_or_else(|| {
                StoreError::Failed(
                    "this directory isn't a project, so it has no project sign-ins".to_string(),
                )
            }),
        }
    }
}

fn read(
    store: &dyn SignInStore,
    url: &str,
    project: Option<&str>,
) -> Result<Option<Saved>, StoreError> {
    let Some(text) = store.load(url, project)? else {
        return Ok(None);
    };
    serde_json::from_str(&text).map(Some).map_err(|error| {
        StoreError::Failed(format!(
            "the saved sign-in for {url} is unreadable: {error}"
        ))
    })
}

pub(crate) trait WantsSignIn {
    fn transport(&self) -> Option<&DynamicTransportError>;

    fn wants_sign_in(&self) -> bool {
        let Some(transport) = self.transport() else {
            return false;
        };
        let mut current: Option<&(dyn std::error::Error + 'static)> =
            Some(transport.error.as_ref());
        while let Some(error) = current {
            if let Some(http) = error.downcast_ref::<StreamableHttpError<reqwest::Error>>() {
                if matches!(
                    http,
                    StreamableHttpError::AuthRequired(_)
                        | StreamableHttpError::Auth(AuthError::AuthorizationRequired)
                ) {
                    return true;
                }
            }
            current = error.source();
        }
        false
    }
}

impl WantsSignIn for ClientInitializeError {
    fn transport(&self) -> Option<&DynamicTransportError> {
        match self {
            ClientInitializeError::TransportError { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl WantsSignIn for ServiceError {
    fn transport(&self) -> Option<&DynamicTransportError> {
        match self {
            ServiceError::TransportSend(error) => Some(error),
            _ => None,
        }
    }
}

struct Stored {
    store: Arc<dyn SignInStore>,
    url: String,
    project: Option<String>,
    fresh: bool,
}

fn store_error(error: StoreError) -> AuthError {
    AuthError::InternalError(error.to_string())
}

#[async_trait::async_trait]
impl CredentialStore for Stored {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        Ok(
            read(self.store.as_ref(), &self.url, self.project.as_deref())
                .map_err(store_error)?
                .map(|saved| saved.credentials),
        )
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let signed_in_at = if self.fresh {
            None
        } else {
            read(self.store.as_ref(), &self.url, self.project.as_deref())
                .map_err(store_error)?
                .map(|saved| saved.signed_in_at)
        };
        let saved = Saved {
            signed_in_at: signed_in_at.unwrap_or_else(now),
            credentials,
        };
        let text = serde_json::to_string(&saved)
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        self.store
            .save(&self.url, self.project.as_deref(), &text)
            .map_err(store_error)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.store
            .remove(&self.url, self.project.as_deref())
            .map(|_| ())
            .map_err(store_error)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

struct Callback {
    code: String,
    state: String,
    issuer: Option<String>,
}

async fn wait_for_callback(
    listener: &TcpListener,
) -> Result<(TcpStream, Result<Callback, String>)> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let Some(target) = request_target(&mut stream).await else {
            continue;
        };
        let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            respond(
                stream,
                "400 Bad Request",
                "Not a sign-in",
                "This address only takes Dive's sign-in callback.",
            )
            .await;
            continue;
        };
        if url.path() != "/callback" {
            respond(
                stream,
                "404 Not Found",
                "Not a sign-in",
                "This address only takes Dive's sign-in callback.",
            )
            .await;
            continue;
        }
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        if let Some(error) = params.get("error") {
            let refused = match params.get("error_description") {
                Some(description) => format!("{error}: {description}"),
                None => error.clone(),
            };
            return Ok((stream, Err(refused)));
        }
        match (params.get("code"), params.get("state")) {
            (Some(code), Some(state)) => {
                let callback = Callback {
                    code: code.clone(),
                    state: state.clone(),
                    issuer: params.get("iss").cloned(),
                };
                return Ok((stream, Ok(callback)));
            }
            _ => {
                respond(
                    stream,
                    "400 Bad Request",
                    "Not a sign-in",
                    "The callback had no authorization code.",
                )
                .await;
            }
        }
    }
}

async fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut received = Vec::new();
    let mut chunk = [0u8; 1024];
    while !received.windows(4).any(|window| window == b"\r\n\r\n") && received.len() < 16 * 1024 {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        received.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&received);
    let mut parts = text.lines().next()?.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    parts.next().map(str::to_string)
}

async fn respond(mut stream: TcpStream, status: &str, title: &str, detail: &str) {
    let body = page(title, detail);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(error) = stream.write_all(response.as_bytes()).await {
        tracing::debug!(error = %error, "the browser left before the sign-in page was sent");
    }
    let _ = stream.shutdown().await;
}

fn page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title><style>:root{{color-scheme:light dark;font-family:system-ui,sans-serif}}body{{margin:0;min-height:100vh;display:grid;place-items:center;text-align:center}}main{{display:flex;flex-direction:column;gap:6px;padding:24px}}b{{font-size:18px}}span{{opacity:.7}}</style></head><body><main><b>{title}</b><span>{detail}</span></main></body></html>",
        title = escape(title),
        detail = escape(detail),
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_core::secrets::MemorySignIns;

    fn saved(at: u64, scopes: &[&str]) -> String {
        serde_json::to_string(&Saved {
            signed_in_at: at,
            credentials: StoredCredentials::new(
                "client".to_string(),
                None,
                scopes.iter().map(|s| s.to_string()).collect(),
                Some(at),
            ),
        })
        .unwrap()
    }

    const URL: &str = "https://mcp.example/mcp";

    #[test]
    fn a_project_sign_in_wins_over_the_global_one() {
        let store = Arc::new(MemorySignIns::new());
        store
            .save(URL, None, &saved(10, &["read", "write"]))
            .unwrap();
        let in_project = SignIns::new(store.clone(), Some("acme".to_string()));
        assert_eq!(
            in_project.status(URL).unwrap(),
            Some(SignIn {
                scope: SignInScope::Global,
                signed_in_at: 10,
                access: vec!["read".to_string(), "write".to_string()],
            })
        );
        store
            .save(URL, Some("acme"), &saved(20, &["read"]))
            .unwrap();
        assert_eq!(
            in_project.status(URL).unwrap().unwrap().scope,
            SignInScope::Project
        );
        let elsewhere = SignIns::new(store.clone(), Some("other".to_string()));
        assert_eq!(
            elsewhere.status(URL).unwrap().unwrap().scope,
            SignInScope::Global
        );
        assert!(in_project.sign_out(URL, SignInScope::Project).unwrap());
        assert_eq!(in_project.status(URL).unwrap().unwrap().signed_in_at, 10);
    }

    #[test]
    fn a_project_sign_in_never_reaches_another_project() {
        let store = Arc::new(MemorySignIns::new());
        store.save(URL, Some("acme"), &saved(20, &[])).unwrap();
        assert_eq!(
            SignIns::new(store.clone(), Some("other".to_string()))
                .status(URL)
                .unwrap(),
            None
        );
        assert_eq!(SignIns::new(store, None).status(URL).unwrap(), None);
    }

    #[test]
    fn project_scope_needs_a_project() {
        let signs = SignIns::new(Arc::new(MemorySignIns::new()), None);
        assert!(signs.sign_out(URL, SignInScope::Project).is_err());
        assert!(!signs.sign_out(URL, SignInScope::Global).unwrap());
    }

    #[tokio::test]
    async fn a_refresh_keeps_when_the_user_signed_in() {
        let store: Arc<dyn SignInStore> = Arc::new(MemorySignIns::new());
        store.save(URL, None, &saved(10, &["read"])).unwrap();
        let refreshing = Stored {
            store: store.clone(),
            url: URL.to_string(),
            project: None,
            fresh: false,
        };
        refreshing
            .save(StoredCredentials::new(
                "client".to_string(),
                None,
                vec!["read".to_string()],
                Some(99),
            ))
            .await
            .unwrap();
        assert_eq!(
            read(store.as_ref(), URL, None)
                .unwrap()
                .unwrap()
                .signed_in_at,
            10
        );
        let signing_in = Stored {
            fresh: true,
            ..refreshing
        };
        signing_in
            .save(StoredCredentials::new(
                "client".to_string(),
                None,
                vec![],
                Some(99),
            ))
            .await
            .unwrap();
        assert!(
            read(store.as_ref(), URL, None)
                .unwrap()
                .unwrap()
                .signed_in_at
                > 10
        );
    }

    #[tokio::test]
    async fn the_callback_listener_ignores_strays_and_returns_the_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            let get = |path: &'static str| async move {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                stream
                    .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                stream
            };
            let mut stray = get("/favicon.ico").await;
            let mut page = String::new();
            stray.read_to_string(&mut page).await.unwrap();
            assert!(page.starts_with("HTTP/1.1 404"));
            get("/callback?code=abc&state=xyz&iss=https%3A%2F%2Fas.example").await
        });
        let (_stream, answer) = wait_for_callback(&listener).await.unwrap();
        let callback = answer.unwrap();
        assert_eq!(
            (
                callback.code.as_str(),
                callback.state.as_str(),
                callback.issuer.as_deref()
            ),
            ("abc", "xyz", Some("https://as.example"))
        );
        browser.await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_consent_comes_back_as_the_providers_words() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(b"GET /callback?error=access_denied&error_description=The+user+said+no HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut page = String::new();
            let _ = stream.read_to_string(&mut page).await;
        });
        let (_stream, answer) = wait_for_callback(&listener).await.unwrap();
        assert_eq!(
            answer.err().as_deref(),
            Some("access_denied: The user said no")
        );
    }

    #[test]
    fn pages_escape_what_the_provider_sent() {
        assert!(page("<b>", "a & \"b\"").contains("&lt;b&gt;</title>"));
    }

    async fn authorization_server() -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let metadata = serde_json::json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
        })
        .to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let target = request_target(&mut stream).await.unwrap_or_default();
                let response = if target.contains("oauth-authorization-server") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{metadata}",
                        metadata.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        format!("{base}/mcp")
    }

    #[tokio::test]
    async fn a_server_that_takes_no_registrations_needs_an_app() {
        let url = authorization_server().await;
        let sign_ins = SignIns::new(Arc::new(MemorySignIns::new()), None);
        let error = sign_ins
            .sign_in(
                "github",
                &url,
                None,
                SignInScope::Global,
                |_| {},
                std::future::pending(),
            )
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<NeedsApp>().is_some(), "{error:#}");
        assert!(error.to_string().contains("[mcp.github.oauth]"), "{error}");
    }

    #[tokio::test]
    async fn a_configured_app_signs_in_with_its_own_client_and_scopes() {
        let url = authorization_server().await;
        let sign_ins = SignIns::new(Arc::new(MemorySignIns::new()), None);
        let app = McpOAuth {
            client_id: "Ov23liApp".to_string(),
            client_secret: Some("shh".to_string()),
            scopes: vec!["repo".to_string(), "read:org".to_string()],
        };
        let mut opened = None;
        let error = sign_ins
            .sign_in(
                "github",
                &url,
                Some(&app),
                SignInScope::Global,
                |address| opened = Some(address.to_string()),
                std::future::ready(()),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "sign-in cancelled");
        let opened = url::Url::parse(&opened.unwrap()).unwrap();
        let query: HashMap<String, String> = opened.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "Ov23liApp");
        assert_eq!(query["scope"], "repo read:org");
        assert!(query["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert!(!query.contains_key("client_secret"));
    }
}
