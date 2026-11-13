use super::dev_key::{claims, DevKey, TokenRequest, DEV_AUDIENCE, DEV_ISSUER};
use super::{router, AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use dive_core::store::RunStatus;
use dive_runtime::packs::InstalledPacks;
use dive_runtime::runtime::{Mode, Runtime, RuntimeOptions, Workspace};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const MARKER: &str = "tok_SERVE_LEAKCHECK";

struct Harness {
    _root: tempfile::TempDir,
    state: Arc<AppState>,
    key: DevKey,
    env_file: std::path::PathBuf,
}

fn tool(dir: &Path, name: &str, script: &str) {
    std::fs::write(
        dir.join(format!("{name}.yaml")),
        format!(
            "version: 1\nname: {name}\ndescription: Test step.\nkind: exec\ncommand: bash\nargs: [\"-c\", \"{script}\"]\ninput_schema: {{type: object}}\n"
        ),
    )
    .unwrap();
}

fn harness(max_runs: usize, max_runs_per_tenant: usize) -> Harness {
    harness_with(max_runs, max_runs_per_tenant, |_, _, _| {})
}

fn harness_with(
    max_runs: usize,
    max_runs_per_tenant: usize,
    setup: impl FnOnce(&Path, &Path, &mut dive_config::Config),
) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let tools = root.path().join("tools");
    let plans = root.path().join("plans");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::create_dir_all(&plans).unwrap();
    let env_file = root.path().join("env.txt");
    tool(&tools, "stamp", "echo '{\\\"ok\\\": true}'");
    tool(
        &tools,
        "nap",
        &format!("env >> {}; sleep 1; echo '{{}}'", env_file.display()),
    );
    std::fs::write(
        plans.join("stamp.yaml"),
        "version: 2\nidentifier: stamp\nname: Stamp\ndescription: Stamps an issue.\ninput_schema:\n  type: object\n  required: [issue]\n  properties:\n    issue: {type: string}\nsteps:\n- id: a\n  tool_name: user__stamp\n  input: {}\nfinish:\n  output:\n    stamped: '{{a.ok}}'\n",
    )
    .unwrap();
    std::fs::write(
        plans.join("naps.yaml"),
        "version: 2\nidentifier: naps\nname: Naps\ndescription: Two naps in a row.\nsteps:\n- id: a\n  tool_name: user__nap\n  input: {}\n- id: b\n  tool_name: user__nap\n  input: {}\nfinish:\n  output:\n    done: '{{b}}'\n",
    )
    .unwrap();
    let mut config = dive_config::Config::default();
    config.tools.paths = vec![tools.clone()];
    config.plans.paths = vec![plans.clone()];
    config.identity.deployment = Some("test".into());
    config.identity.environment = Some("test".into());
    setup(&tools, &plans, &mut config);
    let options = RuntimeOptions {
        storage_backend: Some("memory".to_string()),
        mode: Mode::Serve,
        ..Default::default()
    };
    let runtime = Arc::new(
        Runtime::with_packs(config, InstalledPacks::default(), options)
            .unwrap()
            .on_workspace(&Workspace::new(root.path())),
    );
    let store = runtime.store().unwrap();
    let key = DevKey::generate().unwrap();
    let verifier = super::auth::Verifier::new(DEV_ISSUER, DEV_AUDIENCE)
        .trust_pem(key.public_pem().as_bytes(), "test")
        .unwrap();
    let serve = dive_config::ServeConfig {
        max_runs,
        max_runs_per_tenant,
        ..Default::default()
    };
    let mut state = AppState::new(runtime, store, verifier, &serve, true);
    Arc::get_mut(&mut state).unwrap().poll = Duration::from_millis(10);
    Harness {
        _root: root,
        state,
        key,
        env_file,
    }
}

impl Harness {
    fn app(&self) -> Router {
        router(self.state.clone())
    }

    fn token_with(&self, plan: Option<&str>, tenant: &str, scope: &str, extra: Value) -> String {
        let mut claims = claims(&TokenRequest {
            issuer: DEV_ISSUER,
            audience: DEV_AUDIENCE,
            plan,
            tenant,
            sub: "ada@example.com",
            scope,
            now: chrono::Utc::now().timestamp(),
        });
        if let (Some(claims), Some(extra)) = (claims.as_object_mut(), extra.as_object()) {
            claims.extend(extra.clone());
        }
        self.key.sign(&claims).unwrap()
    }

    fn start_token(&self, plan: &str, tenant: &str) -> String {
        self.token_with(Some(plan), tenant, "", json!({}))
    }

    fn reader(&self, tenant: &str) -> String {
        self.token_with(None, tenant, "plans:read runs:read runs:cancel", json!({}))
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.app().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.send(
            Request::get(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(&self, path: &str, token: &str, body: &str) -> (StatusCode, Value) {
        self.send(
            Request::post(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn start(&self, plan: &str, tenant: &str, input: Value) -> (StatusCode, Value) {
        let token = self.start_token(plan, tenant);
        self.post(
            &format!("/v1/plans/{plan}/runs"),
            &token,
            &json!({ "input": input }).to_string(),
        )
        .await
    }

    async fn finished(&self, run: &str, tenant: &str) -> Value {
        let reader = self.reader(tenant);
        for _ in 0..500 {
            let (_, body) = self.get(&format!("/v1/runs/{run}"), &reader).await;
            if body["status"] != "running" {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("run {run} never finished");
    }

    async fn sse(
        &self,
        run: &str,
        tenant: &str,
        last: Option<u64>,
    ) -> Vec<(Option<u64>, String, Value)> {
        let mut request = Request::get(format!("/v1/runs/{run}/entries"))
            .header("authorization", format!("Bearer {}", self.reader(tenant)))
            .header("accept", "text/event-stream");
        if let Some(last) = last {
            request = request.header("last-event-id", last.to_string());
        }
        let response = self
            .app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = tokio::time::timeout(Duration::from_secs(20), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        parse_sse(&String::from_utf8_lossy(&bytes))
    }
}

fn parse_sse(text: &str) -> Vec<(Option<u64>, String, Value)> {
    text.split("\n\n")
        .filter_map(|block| {
            let mut id = None;
            let mut event = None;
            let mut data = None;
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("id: ") {
                    id = value.parse().ok();
                } else if let Some(value) = line.strip_prefix("event: ") {
                    event = Some(value.to_string());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(value).ok();
                }
            }
            Some((id, event?, data?))
        })
        .collect()
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn a_request_without_a_valid_token_is_refused_as_invalid_token() {
    let h = harness(4, 4);
    let (status, body) = h
        .send(Request::get("/v1/plans").body(Body::empty()).unwrap())
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "invalid_token")
    );
    let (status, body) = h.get("/v1/plans", "not-a-jwt").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "invalid_token")
    );
    let other = DevKey::generate().unwrap();
    let forged = other
        .sign(&claims(&TokenRequest {
            issuer: DEV_ISSUER,
            audience: DEV_AUDIENCE,
            plan: None,
            tenant: "acme",
            sub: "ada",
            scope: "plans:read",
            now: chrono::Utc::now().timestamp(),
        }))
        .unwrap();
    let (status, body) = h.get("/v1/plans", &forged).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "invalid_token")
    );
}

#[tokio::test]
async fn hmac_and_expired_and_foreign_audience_tokens_are_refused() {
    let h = harness(4, 4);
    let hmac = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({"iss": DEV_ISSUER, "aud": DEV_AUDIENCE, "sub": "ada", "tenant": "acme", "scope": "plans:read", "exp": chrono::Utc::now().timestamp() + 60}),
        &jsonwebtoken::EncodingKey::from_secret(b"secret"),
    )
    .unwrap();
    let (status, _) = h.get("/v1/plans", &hmac).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let expired = h.token_with(
        None,
        "acme",
        "plans:read",
        json!({"exp": chrono::Utc::now().timestamp() - 120}),
    );
    let (status, body) = h.get("/v1/plans", &expired).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("expired"));
    let foreign = h.token_with(None, "acme", "plans:read", json!({"aud": "someone-else"}));
    let (status, _) = h.get("/v1/plans", &foreign).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn plans_are_listed_with_their_schemas_for_a_token_with_plans_read() {
    let h = harness(4, 4);
    let (status, body) = h
        .get(
            "/v1/plans",
            &h.token_with(None, "acme", "runs:read", json!({})),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    let (status, body) = h.get("/v1/plans", &h.reader("acme")).await;
    assert_eq!(status, StatusCode::OK);
    let identifiers: Vec<&str> = body["plans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plan| plan["identifier"].as_str().unwrap())
        .collect();
    assert_eq!(identifiers.len(), 2, "{identifiers:?}");
    assert!(identifiers.contains(&"stamp") && identifiers.contains(&"naps"));
    let (status, body) = h.get("/v1/plans/stamp", &h.reader("acme")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["input_schema"]["required"][0], "issue");
    let (status, body) = h.get("/v1/plans/compose_plan", &h.reader("acme")).await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
}

#[tokio::test]
async fn a_start_runs_the_plan_in_the_background_and_reads_back_completed_with_its_result() {
    let h = harness(4, 4);
    let (status, started) = h.start("stamp", "acme", json!({"issue": "ENG-142"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["status"], "running");
    let run = started["run_id"].as_str().unwrap();
    assert_eq!(started["entries_url"], format!("/v1/runs/{run}/entries"));
    let body = h.finished(run, "acme").await;
    assert_eq!(body["status"], "completed", "{body}");
    assert_eq!(body["outcome"], "ok");
    assert_eq!(body["plan"], "stamp");
    assert_eq!(body["result"]["stamped"], true);
    assert_eq!(body["session_id"], started["session_id"]);
}

#[tokio::test]
async fn each_start_refusal_has_its_own_code() {
    let h = harness(4, 4);
    let token = h.start_token("stamp", "acme");
    let (status, body) = h
        .post("/v1/plans/naps/runs", &token, "{\"input\": {}}")
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    let reader = h.reader("acme");
    let (status, body) = h
        .post("/v1/plans/stamp/runs", &reader, "{\"input\": {}}")
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    let (status, body) = h.post("/v1/plans/stamp/runs", &token, "not json").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "bad_request")
    );
    let (status, body) = h
        .post("/v1/plans/stamp/runs", &token, "{\"title\": \"x\"}")
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "bad_request")
    );
    let (status, body) = h
        .post("/v1/plans/stamp/runs", &token, "{\"input\": {}}")
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
    );
    assert!(!body["error"]["problems"].as_array().unwrap().is_empty());
    let (status, body) = h.start("missing", "acme", json!({})).await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
}

#[tokio::test]
async fn a_reused_jti_is_an_idempotency_violation_naming_the_run_it_started() {
    let h = harness(4, 4);
    let token = h.start_token("stamp", "acme");
    let body = json!({"input": {"issue": "ENG-1"}}).to_string();
    let (first, second) = tokio::join!(
        h.post("/v1/plans/stamp/runs", &token, &body),
        h.post("/v1/plans/stamp/runs", &token, &body)
    );
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::ACCEPTED, StatusCode::CONFLICT]);
    let (started, refused) = if first.0 == StatusCode::ACCEPTED {
        (first.1, second.1)
    } else {
        (second.1, first.1)
    };
    assert_eq!(code(&refused), "idempotency_violation");
    assert_eq!(refused["error"]["run_id"], started["run_id"]);
    assert_eq!(refused["error"]["session_id"], started["session_id"]);
    h.finished(started["run_id"].as_str().unwrap(), "acme")
        .await;
    let (status, again) = h.post("/v1/plans/stamp/runs", &token, &body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(again["error"]["run_id"], started["run_id"]);
}

#[tokio::test]
async fn another_tenants_run_is_not_found_for_status_entries_and_cancel() {
    let h = harness(4, 4);
    let (_, started) = h.start("stamp", "acme", json!({"issue": "ENG-1"})).await;
    let run = started["run_id"].as_str().unwrap();
    h.finished(run, "acme").await;
    let other = h.reader("globex");
    let (status, body) = h.get(&format!("/v1/runs/{run}"), &other).await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    let (status, _) = h.get(&format!("/v1/runs/{run}/entries"), &other).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h.post(&format!("/v1/runs/{run}/cancel"), &other, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_watcher_that_reconnects_with_last_event_id_gets_every_later_entry_once_in_order() {
    let h = harness(4, 4);
    let (_, started) = h.start("naps", "acme", json!({})).await;
    let run = started["run_id"].as_str().unwrap();
    let all = h.sse(run, "acme", None).await;
    let entries: Vec<&(Option<u64>, String, Value)> = all
        .iter()
        .filter(|(_, event, _)| event == "entry")
        .collect();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|(_, _, data)| data["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "run_started",
            "step_started",
            "step_finished",
            "step_started",
            "step_finished",
            "run_ended"
        ]
    );
    let seqs: Vec<u64> = entries.iter().map(|(id, _, _)| id.unwrap()).collect();
    let cut = seqs[2];
    let rest = h.sse(run, "acme", Some(cut)).await;
    let replayed: Vec<u64> = rest
        .iter()
        .filter(|(_, event, _)| event == "entry")
        .map(|(id, _, _)| id.unwrap())
        .collect();
    assert_eq!(replayed, seqs[3..].to_vec());
    let reader = h.reader("acme");
    let (status, body) = h
        .send(
            Request::get(format!("/v1/runs/{run}/entries?after={cut}"))
                .header("authorization", format!("Bearer {reader}"))
                .header("accept", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let json_seqs: Vec<u64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(json_seqs, seqs[3..].to_vec());
}

#[tokio::test]
async fn a_cancel_stops_the_run_after_the_tool_in_progress_and_no_later_step_starts() {
    let h = harness(4, 4);
    let (_, started) = h.start("naps", "acme", json!({})).await;
    let run = started["run_id"].as_str().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, body) = h
        .post(&format!("/v1/runs/{run}/cancel"), &h.reader("acme"), "")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["cancel_requested"], true);
    let body = h.finished(run, "acme").await;
    assert_eq!(body["status"], "cancelled", "{body}");
    assert_eq!(body["outcome"], Value::Null);
    let events = h.sse(run, "acme", None).await;
    let kinds: Vec<&str> = events
        .iter()
        .filter(|(_, event, _)| event == "entry")
        .map(|(_, _, data)| data["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "step_started").count(),
        1,
        "{kinds:?}"
    );
    assert!(kinds.contains(&"cancel_requested"), "{kinds:?}");
    let (status, again) = h
        .post(&format!("/v1/runs/{run}/cancel"), &h.reader("acme"), "")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["status"], "cancelled");
}

#[tokio::test]
async fn at_max_runs_readiness_turns_false_and_starts_get_at_capacity() {
    let h = harness(1, 1);
    let ready = |h: &Harness| {
        let app = h.app();
        async move {
            app.oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(ready(&h).await, StatusCode::OK);
    let (_, started) = h.start("naps", "acme", json!({})).await;
    assert_eq!(ready(&h).await, StatusCode::SERVICE_UNAVAILABLE);
    let response = h
        .app()
        .oneshot(
            Request::post("/v1/plans/naps/runs")
                .header(
                    "authorization",
                    format!("Bearer {}", h.start_token("naps", "globex")),
                )
                .header("content-type", "application/json")
                .body(Body::from("{\"input\": {}}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "5");
    h.finished(started["run_id"].as_str().unwrap(), "acme")
        .await;
    assert_eq!(ready(&h).await, StatusCode::OK);
}

#[tokio::test]
async fn draining_interrupts_running_plan_runs_at_a_step_boundary() {
    let h = harness(4, 4);
    let (_, started) = h.start("naps", "acme", json!({})).await;
    let run = started["run_id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.state.drain();
    assert!(!h.state.ready());
    tokio::time::timeout(Duration::from_secs(5), h.state.wait_for_runs())
        .await
        .unwrap();
    let meta = h.state.store.get_run(&run).await.unwrap().unwrap();
    assert_eq!(meta.status, RunStatus::Interrupted);
    let (status, body) = h.start("stamp", "acme", json!({"issue": "x"})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::SERVICE_UNAVAILABLE, "shutting_down")
    );
}

#[tokio::test]
async fn the_run_token_never_reaches_entries_or_exec_tool_environments() {
    let h = harness(4, 4);
    let token = h.token_with(Some("naps"), "acme", "", json!({"marker": MARKER}));
    let (status, started) = h
        .post("/v1/plans/naps/runs", &token, "{\"input\": {}}")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let run = started["run_id"].as_str().unwrap();
    h.finished(run, "acme").await;
    let session = started["session_id"].as_str().unwrap();
    let entries = h.state.store.load_entries(session, None).await.unwrap();
    let logged = serde_json::to_string(&entries).unwrap();
    assert!(!logged.contains(&token));
    assert!(!logged.contains(MARKER));
    let env = std::fs::read_to_string(&h.env_file).unwrap();
    assert!(!env.is_empty());
    assert!(!env.contains(&token));
    assert!(!env.contains(MARKER));
}

#[tokio::test]
async fn health_is_always_ok_and_docs_exist_only_when_enabled() {
    let h = harness(4, 4);
    let status = |path: &'static str, app: Router| async move {
        app.oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    };
    assert_eq!(status("/healthz", h.app()).await, StatusCode::OK);
    assert_eq!(status("/docs", h.app()).await, StatusCode::OK);
    let (code, doc) = h
        .send(Request::get("/openapi.json").body(Body::empty()).unwrap())
        .await;
    assert_eq!(code, StatusCode::OK);
    assert!(doc["paths"]["/v1/plans/stamp/runs"]["post"].is_object());
    let mut hidden = harness(4, 4);
    Arc::get_mut(&mut hidden.state).unwrap().docs = false;
    assert_eq!(status("/docs", hidden.app()).await, StatusCode::NOT_FOUND);
    assert_eq!(
        status("/openapi.json", hidden.app()).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_token_not_yet_valid_or_with_an_unsafe_jti_is_refused() {
    let h = harness(4, 4);
    let early = h.token_with(
        None,
        "acme",
        "plans:read",
        json!({"nbf": chrono::Utc::now().timestamp() + 600}),
    );
    let (status, _) = h.get("/v1/plans", &early).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let odd = h.token_with(Some("stamp"), "acme", "", json!({"jti": "a/b?c"}));
    let (status, body) = h
        .post(
            "/v1/plans/stamp/runs",
            &odd,
            "{\"input\": {\"issue\": \"x\"}}",
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "invalid_token")
    );
}

#[tokio::test]
async fn a_jti_another_tenant_used_is_refused_without_naming_its_run() {
    let h = harness(4, 4);
    let jti = json!({"jti": "shared-jti-1"});
    let first = h.token_with(Some("stamp"), "acme", "", jti.clone());
    let (status, _) = h
        .post(
            "/v1/plans/stamp/runs",
            &first,
            "{\"input\": {\"issue\": \"x\"}}",
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let second = h.token_with(Some("stamp"), "globex", "", jti);
    let (status, body) = h
        .post(
            "/v1/plans/stamp/runs",
            &second,
            "{\"input\": {\"issue\": \"x\"}}",
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "idempotency_violation")
    );
    assert!(body["error"].get("run_id").is_none());
    assert!(body["error"].get("session_id").is_none());
}

#[tokio::test]
async fn forcing_ends_a_run_stuck_in_a_tool_as_interrupted() {
    let h = harness(4, 4);
    let (_, started) = h.start("naps", "acme", json!({})).await;
    let run = started["run_id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.state.drain();
    h.state.force();
    tokio::time::timeout(Duration::from_millis(500), h.state.wait_for_runs())
        .await
        .unwrap();
    let meta = h.state.store.get_run(&run).await.unwrap().unwrap();
    assert_eq!(meta.status, RunStatus::Interrupted);
}

#[tokio::test]
async fn the_reaper_ends_a_run_whose_writer_went_away() {
    let h = harness(4, 4);
    let now = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
    let clock = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(std::sync::atomic::Ordering::SeqCst))
    };
    let store: Arc<dyn dive_core::Store> = Arc::new(dive_store::MemoryStore::with_clock(clock));
    let state = AppState::new(
        h.state.runtime.clone(),
        store.clone(),
        h.state.verifier.clone(),
        &dive_config::ServeConfig::default(),
        false,
    );
    let identity =
        dive_runtime::identity::serve(&h.state.runtime.config.identity, "ada", "acme").unwrap();
    let session = store
        .create_session(dive_core::store::NewSession::plan(
            "Naps", "naps", &identity,
        ))
        .await
        .unwrap();
    store
        .start_run(dive_core::store::StartRun {
            run_id: "orphan".into(),
            session: session.id.clone(),
            kind: dive_core::store::SessionKind::Plan,
            holder: "gone".into(),
            token_expires_at: None,
            started: dive_core::store::RunStarted {
                input: json!({}),
                identity,
                trace_id: None,
                plan: None,
                synthetic: false,
            },
        })
        .await
        .unwrap();
    state.reap().await;
    assert_eq!(
        store.get_run("orphan").await.unwrap().unwrap().status,
        RunStatus::Running
    );
    now.fetch_add(
        dive_core::store::LEASE_EXPIRES_AFTER.as_millis() as i64 + 1,
        std::sync::atomic::Ordering::SeqCst,
    );
    state.reap().await;
    assert_eq!(
        store.get_run("orphan").await.unwrap().unwrap().status,
        RunStatus::Interrupted
    );
}

#[derive(Clone, Default)]
struct AppCalls(Arc<std::sync::Mutex<Vec<axum::http::HeaderMap>>>);

async fn app_server() -> (String, AppCalls) {
    let calls = AppCalls::default();
    let seen = calls.clone();
    let app = Router::new().route(
        "/dive/tools/lookup",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.0.lock().unwrap().push(headers);
                axum::Json(json!({"found": true}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), calls)
}

fn http_harness(base: &str, trusted: bool) -> Harness {
    let base = base.to_string();
    harness_with(4, 4, move |tools, plans, config| {
        std::fs::write(
            tools.join("lookup.yaml"),
            format!("version: 5\nname: lookup\ndescription: Looks up an issue.\nkind: http\nurl: {base}/dive/tools/lookup\ninput_schema: {{type: object}}\n"),
        )
        .unwrap();
        std::fs::write(
            plans.join("lookup.yaml"),
            "version: 2\nidentifier: lookup\nname: Lookup\ndescription: Looks up.\nsteps:\n- id: a\n  tool_name: user__lookup\n  input: {issue: ENG-1}\nfinish:\n  output:\n    found: '{{a.found}}'\n",
        )
        .unwrap();
        if trusted {
            config.http.token_origins = vec![base];
        }
    })
}

fn sent(calls: &AppCalls, name: &str) -> Option<String> {
    calls.0.lock().unwrap()[0]
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tokio::test]
async fn an_http_tool_receives_the_start_token_as_its_bearer_token() {
    let (base, calls) = app_server().await;
    let h = http_harness(&base, true);
    let token = h.start_token("lookup", "acme");
    let (_, started) = h
        .post("/v1/plans/lookup/runs", &token, "{\"input\": {}}")
        .await;
    h.finished(started["run_id"].as_str().unwrap(), "acme")
        .await;
    assert_eq!(
        sent(&calls, "authorization"),
        Some(format!("Bearer {token}"))
    );
}

#[tokio::test]
async fn an_http_tool_receives_the_run_id_and_step_path() {
    let (base, calls) = app_server().await;
    let h = http_harness(&base, true);
    let (_, started) = h.start("lookup", "acme", json!({})).await;
    let run = started["run_id"].as_str().unwrap().to_string();
    let done = h.finished(&run, "acme").await;
    assert_eq!(
        (
            done["status"].as_str(),
            sent(&calls, "dive-run-id"),
            sent(&calls, "dive-call-path")
        ),
        (Some("completed"), Some(run), Some("step:a".to_string()))
    );
}

#[tokio::test]
async fn an_untrusted_origin_never_receives_the_run_token() {
    let (base, calls) = app_server().await;
    let h = http_harness(&base, false);
    let (_, started) = h.start("lookup", "acme", json!({})).await;
    h.finished(started["run_id"].as_str().unwrap(), "acme")
        .await;
    assert_eq!(sent(&calls, "authorization"), None);
}

#[tokio::test]
async fn the_token_an_http_tool_receives_never_reaches_the_entries() {
    let (base, _calls) = app_server().await;
    let h = http_harness(&base, true);
    let token = h.token_with(Some("lookup"), "acme", "", json!({"marker": MARKER}));
    let (_, started) = h
        .post("/v1/plans/lookup/runs", &token, "{\"input\": {}}")
        .await;
    h.finished(started["run_id"].as_str().unwrap(), "acme")
        .await;
    let session = started["session_id"].as_str().unwrap();
    let entries = h.state.store.load_entries(session, None).await.unwrap();
    let logged = serde_json::to_string(&entries).unwrap();
    assert!(!logged.contains(&token) && !logged.contains(MARKER));
}
