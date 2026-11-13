use crate::run::{Run, RunToken};
use crate::secrets::SecretSource;
use crate::template::Roots;
use crate::tools::ToolOutcome;
use crate::user_tools::{expand_env, ExecOutput, HttpMethod, ToolKind};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, RETRY_AFTER};
use reqwest::StatusCode;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const ERROR_EXCERPT_BYTES: usize = 1024;

const PATH_SPLICE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub struct HttpTool<'a> {
    pub name: &'a str,
    pub method: HttpMethod,
    pub url: &'a str,
    pub query: &'a BTreeMap<String, String>,
    pub headers: &'a BTreeMap<String, String>,
    pub body: Option<&'a Value>,
    pub timeout_secs: Option<u64>,
    pub attempts: u32,
    pub output: ExecOutput,
    pub max_response_bytes: Option<u64>,
}

impl<'a> HttpTool<'a> {
    pub fn of(name: &'a str, kind: &'a ToolKind) -> Option<Self> {
        let ToolKind::Http {
            method,
            url,
            query,
            headers,
            body,
            timeout_secs,
            retry,
            output,
            max_response_bytes,
        } = kind
        else {
            return None;
        };
        Some(Self {
            name,
            method: *method,
            url,
            query,
            headers,
            body: body.as_ref(),
            timeout_secs: *timeout_secs,
            attempts: retry.map_or(1, |retry| retry.attempts),
            output: *output,
            max_response_bytes: *max_response_bytes,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct TokenOrigins(Vec<String>);

impl TokenOrigins {
    pub fn new(origins: &[String]) -> Self {
        Self(
            origins
                .iter()
                .filter_map(|origin| origin_of(origin))
                .collect(),
        )
    }

    pub fn trusts(&self, url: &url::Url) -> bool {
        let origin = url.origin().ascii_serialization();
        self.0.contains(&origin)
    }
}

fn origin_of(text: &str) -> Option<String> {
    url::Url::parse(text)
        .ok()
        .map(|url| url.origin().ascii_serialization())
        .filter(|origin| origin != "null")
}

pub struct HttpClient {
    client: reqwest::Client,
    backoff: Duration,
}

impl HttpClient {
    pub fn new(backoff: Duration) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("dive/{}", crate::format::dive_version()))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { client, backoff })
    }
}

fn tool_error(message: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        result: json!({"error": message.into()}),
        is_error: true,
    }
}

pub fn idempotency_key(run_id: &str, path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(run_id.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("dive-{hex}")
}

fn render_url(
    template: &str,
    roots: &Roots<'_>,
    secrets: &dyn SecretSource,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&expand_env(&rest[..start], secrets)?);
        let after = &rest[start..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("url: unterminated {{{{ in {template:?}"))?;
        let tag = &after[..end + 2];
        let value = crate::template::render_str(tag, roots).map_err(|e| format!("url: {e}"))?;
        out.extend(utf8_percent_encode(&value, PATH_SPLICE));
        rest = &after[end + 2..];
    }
    out.push_str(&expand_env(rest, secrets)?);
    Ok(out)
}

fn is_single_tag(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("{{") && trimmed.ends_with("}}") && trimmed.matches("{{").count() == 1
}

fn render_query(
    query: &BTreeMap<String, String>,
    roots: &Roots<'_>,
) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::with_capacity(query.len());
    for (name, template) in query {
        if is_single_tag(template) {
            match crate::template::render_input(&Value::String(template.clone()), roots) {
                Ok(Value::Null) | Err(_) => continue,
                Ok(Value::String(text)) => pairs.push((name.clone(), text)),
                Ok(other) => pairs.push((name.clone(), other.to_string())),
            }
        } else {
            let text = crate::template::render_str(template, roots)
                .map_err(|e| format!("query.{name}: {e}"))?;
            pairs.push((name.clone(), text));
        }
    }
    Ok(pairs)
}

fn header_map(
    headers: &BTreeMap<String, String>,
    secrets: &dyn SecretSource,
) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("headers.{name}: not a valid header name"))?;
        let expanded = expand_env(value, secrets)?;
        let mut value = HeaderValue::from_str(&expanded)
            .map_err(|_| format!("headers.{name}: not a valid header value"))?;
        value.set_sensitive(true);
        map.insert(header, value);
    }
    Ok(map)
}

struct Prepared {
    method: reqwest::Method,
    url: url::Url,
    headers: HeaderMap,
    body: Option<Value>,
}

fn prepare(
    tool: &HttpTool<'_>,
    input: &Value,
    roots: &Roots<'_>,
    secrets: &dyn SecretSource,
) -> Result<Prepared, String> {
    let rendered = render_url(tool.url, roots, secrets)?;
    let mut url = url::Url::parse(&rendered).map_err(|e| format!("url {rendered:?}: {e}"))?;
    let pairs = render_query(tool.query, roots)?;
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    let body = match (tool.method.sends_body(), tool.body) {
        (false, _) => None,
        (true, None) => Some(input.clone()),
        (true, Some(template)) => {
            Some(crate::template::render_input(template, roots).map_err(|e| format!("body: {e}"))?)
        }
    };
    Ok(Prepared {
        method: match tool.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Delete => reqwest::Method::DELETE,
        },
        url,
        headers: header_map(tool.headers, secrets)?,
        body,
    })
}

fn add_run_headers(
    headers: &mut HeaderMap,
    url: &url::Url,
    origins: &TokenOrigins,
) -> Result<(), String> {
    let run = Run::current();
    if let Some(token) = run.as_ref().and_then(|run| run.token.as_ref()) {
        if origins.trusts(url) {
            add_bearer(headers, token)?;
        }
    }
    let run_id = run
        .as_ref()
        .map(|run| run.id.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let path = crate::run::call_path().unwrap_or_else(|| format!("call:{}", uuid::Uuid::new_v4()));
    insert(headers, "idempotency-key", &idempotency_key(&run_id, &path));
    insert(headers, "dive-run-id", &run_id);
    insert(headers, "dive-call-path", &path);
    if let Some(trace) = run.as_ref().and_then(|run| run.trace.as_ref()) {
        let span = crate::trace::current_span_id().unwrap_or_else(|| trace.parent_id.clone());
        insert(
            headers,
            "traceparent",
            &format!("00-{}-{span}-01", trace.trace_id),
        );
    }
    Ok(())
}

fn add_bearer(headers: &mut HeaderMap, token: &RunToken) -> Result<(), String> {
    if headers.contains_key(AUTHORIZATION) {
        return Err("sets its own Authorization header, and Dive sends the run token in Authorization to this origin; send the tool's credential in another header".to_string());
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .map_err(|_| "the run token isn't a valid header value".to_string())?;
    value.set_sensitive(true);
    headers.insert(AUTHORIZATION, value);
    Ok(())
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

pub async fn call(
    client: &HttpClient,
    tool: HttpTool<'_>,
    input: Value,
    origins: &TokenOrigins,
    secrets: &dyn SecretSource,
) -> ToolOutcome {
    let mut scope = Map::new();
    scope.insert("input".to_string(), input.clone());
    let roots = Roots::new(&scope);
    let mut prepared = match prepare(&tool, &input, &roots, secrets) {
        Ok(prepared) => prepared,
        Err(error) => return tool_error(format!("{}: {error}", tool.name)),
    };
    if let Err(error) = add_run_headers(&mut prepared.headers, &prepared.url, origins) {
        return tool_error(format!("{} {error}", tool.name));
    }
    send_with_retries(client, &tool, &prepared).await
}

struct Failure {
    headline: String,
    detail: Option<String>,
    wait: Option<Duration>,
}

impl Failure {
    fn message(self, attempts: u32) -> String {
        let tries = if attempts > 1 {
            format!(" after {attempts} attempts")
        } else {
            String::new()
        };
        match self.detail {
            Some(detail) => format!("{}{tries}: {detail}", self.headline),
            None => format!("{}{tries}", self.headline),
        }
    }
}

async fn send_with_retries(
    client: &HttpClient,
    tool: &HttpTool<'_>,
    prepared: &Prepared,
) -> ToolOutcome {
    let timeout = Duration::from_secs(tool.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let limit = tool
        .max_response_bytes
        .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES);
    let attempts = tool.attempts.max(1);
    let origin = prepared.url.origin().ascii_serialization();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut request = client
            .client
            .request(prepared.method.clone(), prepared.url.clone())
            .headers(prepared.headers.clone())
            .timeout(timeout);
        if let Some(body) = &prepared.body {
            request = request.json(body);
        }
        let failure = match request.send().await {
            Ok(response) => match settle(tool.name, response, tool.output, limit).await {
                Settled::Done(outcome) => return outcome,
                Settled::Retry(failure) => failure,
            },
            Err(error) if error.is_timeout() => Failure {
                headline: format!(
                    "the app didn't answer {} within {}s",
                    tool.name,
                    timeout.as_secs()
                ),
                detail: None,
                wait: None,
            },
            Err(error) => Failure {
                headline: format!("couldn't reach the app at {origin} for {}", tool.name),
                detail: Some(without_url(&error)),
                wait: None,
            },
        };
        if attempt >= attempts {
            return tool_error(failure.message(attempts));
        }
        let backoff = client.backoff.saturating_mul(1 << (attempt - 1).min(6));
        tokio::time::sleep(failure.wait.unwrap_or(backoff).min(MAX_BACKOFF)).await;
    }
}

fn without_url(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(&format!(" for url ({url})"), "");
    }
    text
}

enum Settled {
    Done(ToolOutcome),
    Retry(Failure),
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

async fn read_limited(
    name: &str,
    response: reqwest::Response,
    limit: u64,
) -> Result<Vec<u8>, String> {
    let too_large = || format!("{name} answered with more than {limit} bytes");
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("reading {name}'s answer: {}", without_url(&e)))?
    {
        if body.len() as u64 + chunk.len() as u64 > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Refused,
    InvalidInput,
    Unavailable,
    TimedOut,
    Internal,
    Unauthorized,
    UnknownTool,
    Other,
}

impl Class {
    fn of(status: StatusCode, code: Option<&str>) -> Self {
        match code {
            Some("refused") => return Self::Refused,
            Some("invalid_input") => return Self::InvalidInput,
            Some("unavailable") => return Self::Unavailable,
            Some("timeout") => return Self::TimedOut,
            Some("internal") => return Self::Internal,
            Some("unauthorized") => return Self::Unauthorized,
            Some("unknown_tool") => return Self::UnknownTool,
            _ => {}
        }
        match status {
            StatusCode::UNPROCESSABLE_ENTITY => Self::Refused,
            StatusCode::BAD_REQUEST => Self::InvalidInput,
            StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE => Self::Unavailable,
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => Self::TimedOut,
            StatusCode::INTERNAL_SERVER_ERROR => Self::Internal,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Self::Unauthorized,
            _ => Self::Other,
        }
    }
}

struct AppError {
    code: Option<String>,
    message: String,
    reference: Option<String>,
}

fn app_error(status: StatusCode, body: &[u8]) -> AppError {
    let error = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").cloned());
    let field = |name: &str| {
        error
            .as_ref()
            .and_then(|error| error.get(name))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let message = field("message").unwrap_or_else(|| {
        let end = body.len().min(ERROR_EXCERPT_BYTES);
        let excerpt = String::from_utf8_lossy(&body[..end]);
        match excerpt.trim() {
            "" => format!("HTTP {status}"),
            excerpt => excerpt.to_string(),
        }
    });
    AppError {
        code: field("code"),
        message,
        reference: field("ref"),
    }
}

fn failure(name: &str, status: StatusCode, error: AppError, wait: Option<Duration>) -> Settled {
    let class = Class::of(status, error.code.as_deref());
    let message = error.message;
    let done = |text: String| Settled::Done(tool_error(text));
    match class {
        Class::Refused => done(format!("{name} refused: {message}")),
        Class::InvalidInput => done(format!("{name} rejected its input: {message}")),
        Class::Unauthorized => done(format!(
            "the app refused the run token for {name}: {message}"
        )),
        Class::UnknownTool => done(format!(
            "the app has no tool {name}; regenerate the tool files"
        )),
        Class::Internal => done(match error.reference {
            Some(reference) => format!(
                "{name} failed with an internal error in the app (ref {reference}); retrying won't help"
            ),
            None => format!(
                "{name} failed with an internal error in the app: {message}; retrying won't help"
            ),
        }),
        Class::Other => done(format!("{name} failed (HTTP {status}): {message}")),
        Class::Unavailable => Settled::Retry(Failure {
            headline: format!("{name} is unavailable"),
            detail: Some(message),
            wait,
        }),
        Class::TimedOut => Settled::Retry(Failure {
            headline: format!("{name} timed out"),
            detail: Some(message),
            wait,
        }),
    }
}

async fn settle(
    name: &str,
    response: reqwest::Response,
    output: ExecOutput,
    limit: u64,
) -> Settled {
    let status = response.status();
    if status.is_redirection() {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("nowhere")
            .to_string();
        return Settled::Done(tool_error(format!(
            "{name} redirected to {location}; http tools don't follow redirects"
        )));
    }
    let wait = retry_after(response.headers());
    let body = match read_limited(name, response, limit).await {
        Ok(body) => body,
        Err(error) => return Settled::Done(tool_error(error)),
    };
    if status.is_success() {
        return Settled::Done(success(name, &body, output));
    }
    failure(name, status, app_error(status, &body), wait)
}

fn success(name: &str, body: &[u8], output: ExecOutput) -> ToolOutcome {
    match output {
        ExecOutput::Text => ToolOutcome {
            result: json!({"text": String::from_utf8_lossy(body)}),
            is_error: false,
        },
        ExecOutput::Json if body.iter().all(u8::is_ascii_whitespace) => ToolOutcome {
            result: json!({}),
            is_error: false,
        },
        ExecOutput::Json => match serde_json::from_slice(body) {
            Ok(result) => ToolOutcome {
                result,
                is_error: false,
            },
            Err(error) => tool_error(format!(
                "{name} answered with something that isn't JSON: {error}"
            )),
        },
    }
}

#[cfg(test)]
#[path = "http_tool_tests.rs"]
mod tests;
