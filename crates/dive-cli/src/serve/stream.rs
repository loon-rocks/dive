use super::auth::Caller;
use super::error::{ErrorBody, ServeError};
use super::runs::owned_run;
use super::{AppState, Delta};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dive_core::store::{Entry, StoreError};
use futures::stream::{BoxStream, Stream, StreamExt};
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;
use utoipa::IntoParams;

pub const LAST_EVENT_ID: &str = "last-event-id";

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct EntryDoc {
    pub seq: u64,
    pub run_id: String,
    #[schema(example = "step:E3")]
    pub path: Option<String>,
    pub parent: Option<u64>,
    pub at: i64,
    #[schema(example = "run")]
    pub author: String,
    #[schema(example = "step_finished")]
    pub kind: String,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EntriesQuery {
    pub after: Option<u64>,
}

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| {
            accept.contains("application/json") && !accept.contains("text/event-stream")
        })
}

fn last_event_id(headers: &HeaderMap) -> Result<Option<u64>, ServeError> {
    match headers.get(LAST_EVENT_ID) {
        None => Ok(None),
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .map(Some)
            .ok_or_else(|| ServeError::BadRequest("Last-Event-ID must be an entry seq".into())),
    }
}

#[utoipa::path(
    get,
    path = "/v1/runs/{run}/entries",
    tag = "runs",
    params(
        ("run" = String, Path, description = "The run's id"),
        ("Last-Event-ID" = Option<u64>, Header, description = "Replay the entries after this seq, then follow"),
        EntriesQuery,
    ),
    security(("run_token" = ["runs:read"])),
    responses(
        (status = 200, description = "Server-sent events: `entry` events with `id` set to the entry's seq, `delta` events carrying text as it streams, and `: heartbeat` comments. The stream closes after the `run_ended` entry. With `Accept: application/json`, the entries after `?after=` as a JSON array instead.",
            content(
                (String = "text/event-stream"),
                (Vec<EntryDoc> = "application/json"),
            )),
        (status = 400, description = "bad_request", body = ErrorBody),
        (status = 401, description = "invalid_token", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_found", body = ErrorBody),
    )
)]
pub async fn entries(
    State(state): State<Arc<AppState>>,
    Path(run): Path<String>,
    Query(query): Query<EntriesQuery>,
    headers: HeaderMap,
    caller: Caller,
) -> Result<Response, ServeError> {
    caller.claims.require("runs:read")?;
    let meta = owned_run(&state, &caller.claims, &run).await?;
    if wants_json(&headers) {
        let after = query
            .after
            .unwrap_or(0)
            .max(meta.first_seq.saturating_sub(1));
        let entries: Vec<Entry> = state
            .store
            .load_entries(&meta.session, Some(after))
            .await?
            .into_iter()
            .filter(|entry| entry.run_id == meta.id)
            .collect();
        return Ok(Json(entries).into_response());
    }
    let after = last_event_id(&headers)?.or(query.after);
    let entries = dive_runtime::ops::run::follow(state.store.clone(), &meta, after, state.poll);
    let events = watch(entries.boxed(), state.deltas(&run));
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::new().interval(state.heartbeat).text("heartbeat"))
        .into_response())
}

struct Watch {
    entries: BoxStream<'static, Result<Entry, StoreError>>,
    deltas: Option<broadcast::Receiver<Delta>>,
}

async fn next_delta(deltas: &mut Option<broadcast::Receiver<Delta>>) -> Option<Delta> {
    loop {
        let Some(receiver) = deltas.as_mut() else {
            return std::future::pending().await;
        };
        match receiver.recv().await {
            Ok(delta) => return Some(delta),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => {
                *deltas = None;
            }
        }
    }
}

fn entry_event(entry: &Entry) -> Event {
    Event::default()
        .event("entry")
        .id(entry.seq.to_string())
        .json_data(entry)
        .unwrap_or_else(|_| Event::default().event("entry").id(entry.seq.to_string()))
}

pub fn watch(
    entries: BoxStream<'static, Result<Entry, StoreError>>,
    deltas: Option<broadcast::Receiver<Delta>>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    futures::stream::unfold(Watch { entries, deltas }, |mut watch| async move {
        tokio::select! {
            biased;
            next = watch.entries.next() => match next {
                Some(Ok(entry)) => Some((Ok(entry_event(&entry)), watch)),
                Some(Err(error)) => {
                    tracing::error!(error = %error, "couldn't read a run's entries");
                    None
                }
                None => None,
            },
            Some(delta) = next_delta(&mut watch.deltas) => {
                let event = Event::default()
                    .event("delta")
                    .json_data(&delta)
                    .unwrap_or_else(|_| Event::default().event("delta"));
                Some((Ok(event), watch))
            }
        }
    })
}
