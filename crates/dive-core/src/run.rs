use crate::usage::{RunLedger, UsageReport};
use crate::EventSink;
use serde::Serialize;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

tokio::task_local! {
    static RUN: Arc<Run>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<String> for RunId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for RunId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RunToken(String);

impl RunToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RunToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RunToken(redacted)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceParent {
    pub trace_id: String,
    pub parent_id: String,
}

impl TraceParent {
    pub fn new(trace_id: impl Into<String>, parent_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            parent_id: parent_id.into(),
        }
    }

    pub fn parse(header: &str) -> Option<Self> {
        let mut parts = header.trim().split('-');
        let version = parts.next()?;
        let trace_id = parts.next()?;
        let parent_id = parts.next()?;
        let flags = parts.next()?;
        let hex = |part: &str, len: usize| {
            part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit())
        };
        let zero = |part: &str| part.bytes().all(|b| b == b'0');
        let valid = hex(version, 2)
            && version != "ff"
            && hex(trace_id, 32)
            && !zero(trace_id)
            && hex(parent_id, 16)
            && !zero(parent_id)
            && hex(flags, 2);
        valid.then(|| Self::new(trace_id, parent_id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Identity {
    pub user: String,
    pub tenant: String,
    pub deployment: String,
    pub environment: String,
}

pub struct Run {
    pub id: RunId,
    pub identity: Identity,
    pub token: Option<RunToken>,
    pub trace: Option<TraceParent>,
    ledger: RunLedger,
    events: Arc<dyn EventSink>,
}

impl fmt::Debug for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Run")
            .field("id", &self.id)
            .field("identity", &self.identity)
            .field("token", &self.token)
            .field("trace", &self.trace)
            .finish_non_exhaustive()
    }
}

impl Run {
    pub fn new(identity: Identity, events: Arc<dyn EventSink>) -> Self {
        Self {
            id: RunId::new(),
            identity,
            token: None,
            trace: None,
            ledger: RunLedger::default(),
            events,
        }
    }

    pub fn with_id(mut self, id: RunId) -> Self {
        self.id = id;
        self
    }

    pub fn with_token(mut self, token: RunToken) -> Self {
        self.token = Some(token);
        self
    }

    pub fn with_trace(mut self, trace: TraceParent) -> Self {
        self.trace = Some(trace);
        self
    }

    pub fn events(&self) -> &Arc<dyn EventSink> {
        &self.events
    }

    pub async fn scope<F: Future>(self: Arc<Self>, future: F) -> F::Output {
        RUN.scope(self, future).await
    }

    pub fn current() -> Option<Arc<Run>> {
        RUN.try_with(Arc::clone).ok()
    }

    pub fn usage(&self) -> UsageReport {
        self.ledger.report()
    }

    pub(crate) fn ledger(&self) -> &RunLedger {
        &self.ledger
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::marker::PhantomData;

    fn run() -> Arc<Run> {
        Arc::new(Run::new(
            Identity {
                user: "ada".into(),
                tenant: "local".into(),
                deployment: "cli".into(),
                environment: "local".into(),
            },
            Arc::new(crate::NullSink),
        ))
    }

    #[tokio::test]
    async fn the_run_in_scope_is_current_and_none_is_outside() {
        assert!(Run::current().is_none());
        let outer = run();
        let id = outer.id.clone();
        let seen = outer
            .scope(async {
                tokio::task::yield_now().await;
                Run::current().map(|run| run.id.clone())
            })
            .await;
        assert_eq!(seen, Some(id));
        assert!(Run::current().is_none());
    }

    #[tokio::test]
    async fn interleaved_runs_on_one_task_each_see_their_own() {
        use futures::stream::{self, StreamExt};
        let runs: Vec<Arc<Run>> = (0..4).map(|_| run()).collect();
        let seen: Vec<(RunId, RunId)> = stream::iter(runs)
            .map(|run| async move {
                let id = run.id.clone();
                run.scope(async move {
                    tokio::task::yield_now().await;
                    let current = Run::current().map(|run| run.id.clone()).unwrap_or_default();
                    (id, current)
                })
                .await
            })
            .buffered(4)
            .collect()
            .await;
        for (expected, current) in seen {
            assert_eq!(expected, current);
        }
    }

    #[test]
    fn run_ids_are_unique_uuids() {
        let a = RunId::new();
        let b = RunId::new();
        assert_ne!(a, b);
        assert!(uuid::Uuid::parse_str(a.as_str()).is_ok());
    }

    #[test]
    fn a_token_debugs_redacted_and_exposes_only_on_request() {
        let token = RunToken::new("tok_LEAKCHECK");
        assert_eq!(format!("{token:?}"), "RunToken(redacted)");
        assert_eq!(token.expose(), "tok_LEAKCHECK");
        let run = Run::new(Identity::default(), Arc::new(crate::NullSink)).with_token(token);
        assert!(!format!("{run:?}").contains("tok_LEAKCHECK"));
    }

    trait Fallback {
        fn serializes(&self) -> bool {
            false
        }
        fn displays(&self) -> bool {
            false
        }
    }

    impl<T> Fallback for PhantomData<T> {}

    struct Probe<T>(PhantomData<T>);

    impl<T> std::ops::Deref for Probe<T> {
        type Target = PhantomData<T>;
        fn deref(&self) -> &PhantomData<T> {
            &self.0
        }
    }

    impl<T: Serialize> Probe<T> {
        fn serializes(&self) -> bool {
            true
        }
    }

    struct DisplayProbe<T>(PhantomData<T>);

    impl<T> std::ops::Deref for DisplayProbe<T> {
        type Target = PhantomData<T>;
        fn deref(&self) -> &PhantomData<T> {
            &self.0
        }
    }

    impl<T: fmt::Display> DisplayProbe<T> {
        fn displays(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_token_has_no_serialize_or_display() {
        assert!(Probe::<String>(PhantomData).serializes());
        assert!(!Probe::<RunToken>(PhantomData).serializes());
        assert!(DisplayProbe::<String>(PhantomData).displays());
        assert!(!DisplayProbe::<RunToken>(PhantomData).displays());
    }

    #[test]
    fn a_traceparent_header_parses_its_ids() {
        let parent =
            TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(parent.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parent.parent_id, "00f067aa0ba902b7");
        assert!(
            TraceParent::parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
        assert!(TraceParent::parse("garbage").is_none());
    }
}
