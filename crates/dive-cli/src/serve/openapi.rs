use super::error::{ErrorBody, ErrorDetail};
use super::plans::{PlanBody, PlanList, StartBody, StartedBody};
use super::runs::RunBody;
use super::AppState;
use axum::extract::State;
use axum::response::Html;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

struct RunToken;

impl Modify for RunToken {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "run_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "A run token your app signs. See the Dive docs, Serve > Run tokens.",
                    ))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Dive",
        description = "Start plan runs, watch their entries and cancel them over HTTP.",
        license(name = "MIT")
    ),
    paths(
        super::plans::list,
        super::plans::show,
        super::plans::start,
        super::runs::show,
        super::stream::entries,
        super::runs::cancel,
    ),
    components(schemas(
        PlanBody,
        PlanList,
        StartBody,
        StartedBody,
        RunBody,
        ErrorBody,
        ErrorDetail,
        super::stream::EntryDoc
    )),
    modifiers(&RunToken),
    tags(
        (name = "plans", description = "The plans this deployment serves"),
        (name = "runs", description = "Start, read, watch and cancel plan runs"),
    )
)]
pub struct ApiDoc;

pub fn generic() -> Value {
    let mut doc = serde_json::to_value(ApiDoc::openapi()).unwrap_or(Value::Null);
    if let Some(info) = doc.get_mut("info") {
        info["version"] = json!("1");
    }
    doc
}

pub fn with_plans(mut doc: Value, plans: &[dive_runtime::ops::run::PlanInfo]) -> Value {
    let Some(template) = doc
        .pointer("/paths/~1v1~1plans~1{plan}~1runs/post")
        .cloned()
    else {
        return doc;
    };
    for plan in plans {
        let mut operation = template.clone();
        if let Some(object) = operation.as_object_mut() {
            object.remove("parameters");
            object.insert(
                "operationId".into(),
                json!(format!("start_{}", plan.identifier)),
            );
            object.insert("summary".into(), json!(format!("Start {}", plan.name)));
            object.insert("description".into(), json!(plan.description));
        }
        let input = plan
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object"}));
        operation["requestBody"] = json!({
            "required": true,
            "content": {"application/json": {"schema": {
                "type": "object",
                "required": ["input"],
                "properties": {
                    "input": input,
                    "title": {"type": "string"}
                }
            }}}
        });
        if let Some(result) = &plan.result_schema {
            operation["x-dive-result-schema"] = result.clone();
        }
        doc["paths"][format!("/v1/plans/{}/runs", plan.identifier)] = json!({ "post": operation });
    }
    doc
}

pub async fn deployment(State(state): State<Arc<AppState>>) -> Json<Value> {
    let plans = dive_runtime::ops::run::plans(&state.runtime);
    Json(with_plans(generic(), &plans))
}

pub async fn page() -> Html<&'static str> {
    Html(PAGE)
}

const PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <title>Dive API</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
  </head>
  <body>
    <script id="api-reference" data-url="/openapi.json"></script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.25.11"></script>
  </body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "docs/openapi.json";

    fn generic_text() -> String {
        let mut text = serde_json::to_string_pretty(&generic()).unwrap_or_default();
        text.push('\n');
        text
    }

    fn doc_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(DOC)
    }

    #[test]
    fn the_committed_openapi_document_matches_the_handlers() {
        let path = doc_path();
        let generated = generic_text();
        if std::env::var(crate::docs_gen::UPDATE_ENV).is_ok_and(|value| value == "1") {
            std::fs::write(&path, &generated).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == generated,
            "{DOC} is out of date; run `DIVE_UPDATE_DOCS=1 cargo test -p dive-cli serve::openapi` to regenerate it"
        );
    }

    #[test]
    fn the_deployment_document_adds_a_typed_start_per_plan() {
        let plans = vec![dive_runtime::ops::run::PlanInfo {
            identifier: "triage".into(),
            name: "Triage".into(),
            description: "Sorts an issue".into(),
            input_schema: Some(
                json!({"type": "object", "required": ["issue"], "properties": {"issue": {"type": "string"}}}),
            ),
            result_schema: Some(json!({"type": "object"})),
        }];
        let doc = with_plans(generic(), &plans);
        let start = &doc["paths"]["/v1/plans/triage/runs"]["post"];
        assert_eq!(start["operationId"], "start_triage");
        assert_eq!(
            start["requestBody"]["content"]["application/json"]["schema"]["properties"]["input"]
                ["required"][0],
            "issue"
        );
        assert!(doc["paths"]["/v1/plans/{plan}/runs"].is_object());
    }
}
