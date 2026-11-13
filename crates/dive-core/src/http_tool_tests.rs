use crate::run::{Identity, Run, RunToken, TraceParent};
use crate::secrets::MemorySecrets;
use crate::user_tools::{parse_tool_source, validate_tool, UserToolRegistry};
use crate::{ToolOutcome, ToolRegistry};
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "tok_SECRET_RUN_TOKEN";

#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Clone)]
struct Reply {
    status: StatusCode,
    headers: Vec<(&'static str, String)>,
    body: String,
}

fn reply(status: u16, body: &str) -> Reply {
    Reply {
        status: StatusCode::from_u16(status).unwrap(),
        headers: Vec::new(),
        body: body.to_string(),
    }
}

#[derive(Clone, Default)]
struct App {
    seen: Arc<Mutex<Vec<Seen>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
}

impl App {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn serve(replies: Vec<Reply>) -> (String, App) {
    let app = App {
        seen: Arc::default(),
        replies: Arc::new(Mutex::new(replies.into())),
    };
    let state = app.clone();
    let router = axum::Router::new().fallback(
        move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let state = state.clone();
            async move {
                state.seen.lock().unwrap().push(Seen {
                    method,
                    uri,
                    headers,
                    body,
                });
                let next = state
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| reply(200, "{}"));
                let mut response: Response = (next.status, next.body).into_response();
                for (name, value) in next.headers {
                    response.headers_mut().insert(name, value.parse().unwrap());
                }
                response
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), app)
}

fn router() -> Arc<dive_llm::ModelRouter> {
    Arc::new(dive_llm::ModelRouter::with_providers(
        HashMap::new(),
        dive_config::ModelRoles::default(),
    ))
}

fn tool(yaml: &str) -> crate::user_tools::UserToolDoc {
    let doc = parse_tool_source(yaml).unwrap();
    validate_tool(&doc).unwrap();
    doc
}

fn registry(base: &str, yaml: &str) -> UserToolRegistry {
    UserToolRegistry::new(vec![tool(&yaml.replace("BASE", base))], router())
        .with_token_origins(&[base.to_string()])
        .with_http_backoff(Duration::from_millis(1))
        .with_secrets(Arc::new(
            MemorySecrets::env()
                .with("APP_KEY", "key-123")
                .with("APP_HOST", base),
        ))
}

fn run() -> Arc<Run> {
    Arc::new(
        Run::new(
            Identity {
                user: "ada@example.com".into(),
                tenant: "acme".into(),
                deployment: "cli".into(),
                environment: "local".into(),
            },
            Arc::new(crate::NullSink),
        )
        .with_token(RunToken::new(TOKEN)),
    )
}

async fn invoke(registry: &UserToolRegistry, input: Value) -> ToolOutcome {
    run()
        .scope(crate::run::in_call(
            "plan:triage/step:E3".to_string(),
            registry.invoke("user__t", input),
        ))
        .await
        .unwrap()
}

fn header<'a>(seen: &'a Seen, name: &str) -> Option<&'a str> {
    seen.headers.get(name).and_then(|value| value.to_str().ok())
}

const POST: &str = "name: t\ndescription: d\nkind: http\nurl: BASE/tools/t\n";

#[tokio::test]
async fn a_post_sends_the_input_as_json() {
    let (base, app) = serve(vec![]).await;
    invoke(&registry(&base, POST), json!({"title": "Broken"})).await;
    let body: Value = serde_json::from_slice(&app.seen()[0].body).unwrap();
    assert_eq!(body, json!({"title": "Broken"}));
}

#[tokio::test]
async fn the_run_token_goes_to_a_trusted_origin_as_a_bearer_token() {
    let (base, app) = serve(vec![]).await;
    invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        header(&app.seen()[0], "authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

#[tokio::test]
async fn every_call_carries_the_run_id_and_call_path() {
    let (base, app) = serve(vec![]).await;
    invoke(&registry(&base, POST), json!({})).await;
    let seen = &app.seen()[0];
    assert_eq!(
        (
            header(seen, "dive-call-path"),
            header(seen, "dive-run-id").map(|id| uuid::Uuid::parse_str(id).is_ok())
        ),
        (Some("plan:triage/step:E3"), Some(true))
    );
}

#[tokio::test]
async fn the_idempotency_key_hashes_the_run_id_and_call_path() {
    let (base, app) = serve(vec![]).await;
    invoke(&registry(&base, POST), json!({})).await;
    let seen = &app.seen()[0];
    let expected = crate::http_tool::idempotency_key(
        header(seen, "dive-run-id").unwrap(),
        "plan:triage/step:E3",
    );
    assert_eq!(header(seen, "idempotency-key"), Some(expected.as_str()));
}

#[tokio::test]
async fn the_user_agent_names_dive_and_its_version() {
    let (base, app) = serve(vec![]).await;
    invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        header(&app.seen()[0], "user-agent"),
        Some(format!("dive/{}", crate::format::dive_version()).as_str())
    );
}

#[tokio::test]
async fn an_untrusted_origin_gets_no_authorization() {
    let (base, app) = serve(vec![]).await;
    let registry = UserToolRegistry::new(vec![tool(&POST.replace("BASE", &base))], router())
        .with_token_origins(&["https://app.example.com".to_string()]);
    invoke(&registry, json!({})).await;
    assert_eq!(header(&app.seen()[0], "authorization"), None);
}

#[tokio::test]
async fn a_traceparent_names_the_runs_trace() {
    let (base, app) = serve(vec![]).await;
    let registry = registry(&base, POST);
    let trace = TraceParent::new("4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7");
    let run = Arc::new(Run::new(Identity::default(), Arc::new(crate::NullSink)).with_trace(trace));
    run.scope(registry.invoke("user__t", json!({})))
        .await
        .unwrap();
    let traceparent = header(&app.seen()[0], "traceparent").unwrap().to_string();
    assert!(
        traceparent.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-")
            && traceparent.ends_with("-01"),
        "{traceparent}"
    );
}

#[tokio::test]
async fn a_path_splice_is_percent_encoded() {
    let (base, app) = serve(vec![]).await;
    let yaml = "name: t\ndescription: d\nkind: http\nmethod: GET\nurl: BASE/issues/{{input.id}}\n";
    invoke(&registry(&base, yaml), json!({"id": "a/b c"})).await;
    assert_eq!(app.seen()[0].uri.path(), "/issues/a%2Fb%20c");
}

#[tokio::test]
async fn query_values_render_and_an_absent_one_is_left_out() {
    let (base, app) = serve(vec![]).await;
    let yaml = "name: t\ndescription: d\nkind: http\nmethod: GET\nurl: BASE/issues\nquery:\n  state: \"{{input.state}}\"\n  label: \"{{input.label}}\"\n";
    invoke(&registry(&base, yaml), json!({"state": "open"})).await;
    assert_eq!(app.seen()[0].uri.query(), Some("state=open"));
}

#[tokio::test]
async fn the_request_uses_the_tools_method() {
    let (base, app) = serve(vec![]).await;
    let yaml = "name: t\ndescription: d\nkind: http\nmethod: PATCH\nurl: BASE/issues\n";
    invoke(&registry(&base, yaml), json!({})).await;
    assert_eq!(app.seen()[0].method, Method::PATCH);
}

#[tokio::test]
async fn a_get_sends_no_body() {
    let (base, app) = serve(vec![]).await;
    let yaml = "name: t\ndescription: d\nkind: http\nmethod: GET\nurl: BASE/issues\n";
    invoke(&registry(&base, yaml), json!({"id": "1"})).await;
    assert!(app.seen()[0].body.is_empty());
}

#[tokio::test]
async fn a_body_template_reshapes_the_input() {
    let (base, app) = serve(vec![]).await;
    let yaml = "name: t\ndescription: d\nkind: http\nurl: BASE/issues\nbody:\n  issue:\n    title: \"{{input.title}}\"\n";
    invoke(&registry(&base, yaml), json!({"title": "Broken"})).await;
    let body: Value = serde_json::from_slice(&app.seen()[0].body).unwrap();
    assert_eq!(body, json!({"issue": {"title": "Broken"}}));
}

#[tokio::test]
async fn secrets_resolve_in_the_url_and_headers() {
    let (base, app) = serve(vec![]).await;
    let yaml = "name: t\ndescription: d\nkind: http\nurl: ${APP_HOST}/issues\nheaders:\n  X-Api-Key: ${APP_KEY}\n";
    invoke(&registry(&base, yaml), json!({})).await;
    assert_eq!(header(&app.seen()[0], "x-api-key"), Some("key-123"));
}

#[tokio::test]
async fn a_json_response_is_the_result() {
    let (base, _app) = serve(vec![reply(200, r#"{"id": "ISS-1"}"#)]).await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        (outcome.result, outcome.is_error),
        (json!({"id": "ISS-1"}), false)
    );
}

#[tokio::test]
async fn an_empty_response_is_an_empty_object() {
    let (base, _app) = serve(vec![reply(204, "")]).await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!((outcome.result, outcome.is_error), (json!({}), false));
}

#[tokio::test]
async fn text_output_wraps_the_body() {
    let (base, _app) = serve(vec![reply(200, "plain words")]).await;
    let yaml = "name: t\ndescription: d\nkind: http\nurl: BASE/t\noutput: text\n";
    let outcome = invoke(&registry(&base, yaml), json!({})).await;
    assert_eq!(outcome.result, json!({"text": "plain words"}));
}

#[tokio::test]
async fn a_4xx_error_message_reaches_the_tool_error() {
    let (base, _app) = serve(vec![reply(
        422,
        r#"{"error": {"message": "the tracker is read-only"}}"#,
    )])
    .await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        (outcome.result, outcome.is_error),
        (
            json!({"error": "t refused: the tracker is read-only"}),
            true
        )
    );
}

#[tokio::test]
async fn a_4xx_without_an_error_body_names_the_status_and_body() {
    let (base, _app) = serve(vec![reply(404, "no such issue")]).await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t failed (HTTP 404 Not Found): no such issue"})
    );
}

#[tokio::test]
async fn a_401_says_the_app_refused_the_run_token() {
    let (base, _app) = serve(vec![reply(401, r#"{"error": {"message": "expired"}}"#)]).await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "the app refused the run token for t: expired"})
    );
}

#[tokio::test]
async fn a_401_is_never_retried() {
    let (base, app) = serve(vec![reply(401, "")]).await;
    let yaml = format!("{POST}retry:\n  attempts: 3\n");
    invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!(app.seen().len(), 1);
}

#[tokio::test]
async fn a_503_then_a_200_completes_after_one_retry() {
    let (base, _app) = serve(vec![reply(503, ""), reply(200, r#"{"ok": true}"#)]).await;
    let yaml = format!("{POST}retry:\n  attempts: 3\n");
    let outcome = invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!(
        (outcome.result, outcome.is_error),
        (json!({"ok": true}), false)
    );
}

#[tokio::test]
async fn a_retry_sends_the_same_idempotency_key() {
    let (base, app) = serve(vec![reply(503, ""), reply(200, "{}")]).await;
    let yaml = format!("{POST}retry:\n  attempts: 3\n");
    invoke(&registry(&base, &yaml), json!({})).await;
    let keys: Vec<String> = app
        .seen()
        .iter()
        .map(|seen| header(seen, "idempotency-key").unwrap().to_string())
        .collect();
    assert_eq!((keys.len(), keys[0] == keys[1]), (2, true));
}

#[tokio::test]
async fn retries_stop_after_the_last_attempt() {
    let (base, _app) = serve(vec![reply(503, ""), reply(503, ""), reply(503, "")]).await;
    let yaml = format!("{POST}retry:\n  attempts: 2\n");
    let outcome = invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t is unavailable after 2 attempts: HTTP 503 Service Unavailable"})
    );
}

#[tokio::test]
async fn without_retry_a_503_fails_at_once() {
    let (base, app) = serve(vec![reply(503, ""), reply(200, "{}")]).await;
    invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(app.seen().len(), 1);
}

#[tokio::test]
async fn a_redirect_is_a_tool_error_naming_the_location() {
    let mut redirect = reply(302, "");
    redirect
        .headers
        .push(("location", "https://elsewhere.example.com/".to_string()));
    let (base, _app) = serve(vec![redirect]).await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t redirected to https://elsewhere.example.com/; http tools don't follow redirects"})
    );
}

#[tokio::test]
async fn a_response_over_the_size_limit_is_a_tool_error() {
    let (base, _app) = serve(vec![reply(200, &"x".repeat(64))]).await;
    let yaml = format!("{POST}max_response_bytes: 16\n");
    let outcome = invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t answered with more than 16 bytes"})
    );
}

#[tokio::test]
async fn a_tool_authorization_header_with_a_run_token_is_refused() {
    let (base, app) = serve(vec![]).await;
    let yaml = format!("{POST}headers:\n  Authorization: Bearer static\n");
    let outcome = invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!((outcome.is_error, app.seen().len()), (true, 0));
}

#[tokio::test]
async fn the_run_token_never_reaches_the_result() {
    let (base, _app) = serve(vec![reply(500, "boom")]).await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert!(!outcome.result.to_string().contains(TOKEN));
}

#[tokio::test]
async fn a_get_tool_is_read_only_unless_it_says_otherwise() {
    let registry = UserToolRegistry::new(
        vec![tool(
            "name: t\ndescription: d\nkind: http\nmethod: GET\nurl: http://app/t\n",
        )],
        router(),
    );
    assert_eq!(registry.tools().await.unwrap()[0].read_only, Some(true));
}

#[tokio::test]
async fn a_post_tool_isnt_read_only_by_default() {
    let registry = UserToolRegistry::new(
        vec![tool(
            "name: t\ndescription: d\nkind: http\nurl: http://app/t\n",
        )],
        router(),
    );
    assert_eq!(registry.tools().await.unwrap()[0].read_only, None);
}

#[test]
fn an_unknown_http_key_is_refused() {
    let error =
        parse_tool_source("name: t\ndescription: d\nkind: http\nurl: http://app/t\ncommand: ls\n")
            .unwrap_err();
    assert!(error.contains("`command`"), "{error}");
}

#[test]
fn a_header_template_is_refused() {
    let doc = parse_tool_source(
        "name: t\ndescription: d\nkind: http\nurl: http://app/t\nheaders:\n  X-Id: \"{{input.id}}\"\n",
    )
    .unwrap();
    assert!(validate_tool(&doc).unwrap_err().contains("headers.X-Id"));
}

#[test]
fn zero_retry_attempts_are_refused() {
    let doc = parse_tool_source(
        "name: t\ndescription: d\nkind: http\nurl: http://app/t\nretry:\n  attempts: 0\n",
    )
    .unwrap();
    assert!(validate_tool(&doc).is_err());
}

#[test]
fn secrets_in_the_url_and_headers_are_needed_secrets() {
    let doc = tool(
        "name: t\ndescription: d\nkind: http\nurl: ${APP_HOST}/t\nheaders:\n  X-Api-Key: ${APP_KEY}\n",
    );
    let names: Vec<String> = crate::user_tools::needed_secrets(&doc)
        .into_iter()
        .map(|secret| secret.name)
        .collect();
    assert_eq!(names, vec!["APP_HOST", "APP_KEY"]);
}

#[test]
fn the_generated_key_is_kept_on_any_kind() {
    let doc = tool("name: t\ndescription: d\ngenerated: dive-go\nkind: http\nurl: http://app/t\n");
    assert_eq!(doc.generated.as_deref(), Some("dive-go"));
}

#[tokio::test]
async fn a_500_is_never_retried() {
    let (base, app) = serve(vec![reply(500, ""), reply(200, "{}")]).await;
    let yaml = format!("{POST}retry:\n  attempts: 3\n");
    invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!(app.seen().len(), 1);
}

#[tokio::test]
async fn an_internal_error_names_its_reference_and_says_retrying_wont_help() {
    let (base, _app) = serve(vec![reply(
        500,
        r#"{"error": {"code": "internal", "message": "the tool failed", "ref": "3f2a9c1e"}}"#,
    )])
    .await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t failed with an internal error in the app (ref 3f2a9c1e); retrying won't help"})
    );
}

#[tokio::test]
async fn an_internal_error_without_a_reference_carries_its_message() {
    let (base, _app) = serve(vec![reply(500, "database is down")]).await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t failed with an internal error in the app: database is down; retrying won't help"})
    );
}

#[tokio::test]
async fn the_error_code_classes_a_failure_over_its_status() {
    let (base, app) = serve(vec![
        reply(
            500,
            r#"{"error": {"code": "unavailable", "message": "locked"}}"#,
        ),
        reply(200, "{}"),
    ])
    .await;
    let yaml = format!("{POST}retry:\n  attempts: 3\n");
    invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!(app.seen().len(), 2);
}

#[tokio::test]
async fn bad_input_says_the_tool_rejected_its_input() {
    let (base, _app) = serve(vec![reply(
        400,
        r#"{"error": {"code": "invalid_input", "message": "title is required"}}"#,
    )])
    .await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t rejected its input: title is required"})
    );
}

#[tokio::test]
async fn an_unknown_tool_code_says_to_regenerate_the_tool_files() {
    let (base, _app) = serve(vec![reply(
        404,
        r#"{"error": {"code": "unknown_tool", "message": "no tool t"}}"#,
    )])
    .await;
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "the app has no tool t; regenerate the tool files"})
    );
}

#[tokio::test]
async fn a_504_is_a_timeout_and_is_retried() {
    let (base, _app) = serve(vec![reply(504, "slow"), reply(504, "slow")]).await;
    let yaml = format!("{POST}retry:\n  attempts: 2\n");
    let outcome = invoke(&registry(&base, &yaml), json!({})).await;
    assert_eq!(
        outcome.result,
        json!({"error": "t timed out after 2 attempts: slow"})
    );
}

#[tokio::test]
async fn an_unreachable_app_names_its_origin() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let outcome = invoke(&registry(&base, POST), json!({})).await;
    let message = outcome.result["error"].as_str().unwrap().to_string();
    assert!(
        message.starts_with(&format!("couldn't reach the app at {base} for t: ")),
        "{message}"
    );
}
