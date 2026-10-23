use async_trait::async_trait;
use dive_core::pipeline::{ErrorDecision, ExecutionGate, GateContext, GateDecision};
use dive_core::{ServerIdentity, ToolDef, ToolError, ToolOutcome, ToolRegistry, ToolServer};
use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

#[derive(Debug, Default)]
struct Inner {
    token: Mutex<CancellationToken>,
    in_flight: AtomicUsize,
    idle: Notify,
}

#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    inner: Arc<Inner>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    fn token(&self) -> CancellationToken {
        self.inner.token.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.token().cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token().is_cancelled()
    }

    pub fn rearm(&self) {
        let mut token = self.inner.token.lock().unwrap();
        if token.is_cancelled() {
            *token = CancellationToken::new();
        }
    }

    pub fn tools_in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::SeqCst)
    }

    pub fn gate(&self, inner: Option<Arc<dyn ExecutionGate>>) -> Arc<dyn ExecutionGate> {
        Arc::new(CancelGate {
            handle: self.clone(),
            inner,
        })
    }

    pub fn track(&self, registry: Arc<dyn ToolRegistry>) -> Arc<dyn ToolRegistry> {
        Arc::new(Tracked {
            handle: self.clone(),
            inner: registry,
        })
    }

    pub async fn run<F: Future>(&self, work: F) -> Result<F::Output, Cancelled> {
        let token = self.token();
        tokio::select! {
            biased;
            _ = self.stopped(token) => Err(Cancelled),
            output = work => Ok(output),
        }
    }

    async fn stopped(&self, token: CancellationToken) {
        token.cancelled().await;
        loop {
            let idle = self.inner.idle.notified();
            if self.tools_in_flight() == 0 {
                return;
            }
            idle.await;
        }
    }
}

struct InFlight(CancelHandle);

impl InFlight {
    fn enter(handle: &CancelHandle) -> Self {
        handle.inner.in_flight.fetch_add(1, Ordering::SeqCst);
        Self(handle.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.0.inner.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.inner.idle.notify_waiters();
        }
    }
}

struct Tracked {
    handle: CancelHandle,
    inner: Arc<dyn ToolRegistry>,
}

#[async_trait]
impl ToolRegistry for Tracked {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        self.inner.tools().await
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let _in_flight = InFlight::enter(&self.handle);
        self.inner.invoke(name, input).await
    }

    async fn servers(&self) -> Vec<ToolServer> {
        self.inner.servers().await
    }

    async fn server_identity(&self, server: &str) -> Option<Result<ServerIdentity, String>> {
        self.inner.server_identity(server).await
    }

    async fn needed_secrets(&self, name: &str) -> Option<Vec<dive_core::secrets::NeededSecret>> {
        self.inner.needed_secrets(name).await
    }
}

pub struct CancelGate {
    handle: CancelHandle,
    inner: Option<Arc<dyn ExecutionGate>>,
}

impl CancelGate {
    pub fn new(token: CancellationToken) -> Self {
        let handle = CancelHandle::new();
        *handle.inner.token.lock().unwrap() = token;
        Self {
            handle,
            inner: None,
        }
    }
}

#[async_trait]
impl ExecutionGate for CancelGate {
    async fn before_tool(&self, ctx: GateContext<'_>) -> GateDecision {
        if self.handle.is_cancelled() {
            return GateDecision::Abort;
        }
        match &self.inner {
            Some(inner) => inner.before_tool(ctx).await,
            None => GateDecision::Proceed,
        }
    }

    async fn on_tool_error(&self, ctx: GateContext<'_>, error: &Value) -> ErrorDecision {
        match &self.inner {
            Some(inner) => inner.on_tool_error(ctx, error).await,
            None => ErrorDecision::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::InstalledPacks;
    use crate::runtime::{PipelineHooks, Runtime, RuntimeOptions};
    use dive_core::NullSink;
    use std::time::{Duration, Instant};

    fn context_parts() -> (
        dive_core::pipeline::StepPath,
        Value,
        serde_json::Map<String, Value>,
    ) {
        (
            dive_core::pipeline::StepPath::top("E1"),
            serde_json::json!({}),
            serde_json::Map::new(),
        )
    }

    #[tokio::test]
    async fn the_gate_aborts_only_once_cancelled_and_rearming_clears_it() {
        let handle = CancelHandle::new();
        let gate = handle.gate(None);
        let (path, input, scope) = context_parts();
        let context = || GateContext {
            path: &path,
            tool_name: "t__search",
            rendered_input: &input,
            call_stack: &[],
            scope: &scope,
        };
        assert!(matches!(
            gate.before_tool(context()).await,
            GateDecision::Proceed
        ));
        handle.cancel();
        assert!(matches!(
            gate.before_tool(context()).await,
            GateDecision::Abort
        ));
        handle.rearm();
        assert!(matches!(
            gate.before_tool(context()).await,
            GateDecision::Proceed
        ));
    }

    struct Refuse;

    #[async_trait]
    impl ExecutionGate for Refuse {
        async fn before_tool(&self, _ctx: GateContext<'_>) -> GateDecision {
            GateDecision::Skip {
                result: serde_json::json!("skipped"),
            }
        }
    }

    #[tokio::test]
    async fn the_gate_defers_to_the_gate_it_wraps_until_cancelled() {
        let handle = CancelHandle::new();
        let gate = handle.gate(Some(Arc::new(Refuse)));
        let (path, input, scope) = context_parts();
        let context = || GateContext {
            path: &path,
            tool_name: "t__search",
            rendered_input: &input,
            call_stack: &[],
            scope: &scope,
        };
        assert!(matches!(
            gate.before_tool(context()).await,
            GateDecision::Skip { .. }
        ));
        handle.cancel();
        assert!(matches!(
            gate.before_tool(context()).await,
            GateDecision::Abort
        ));
    }

    #[tokio::test]
    async fn work_is_dropped_only_while_no_tool_is_in_flight() {
        let handle = CancelHandle::new();
        assert_eq!(handle.run(async { 7 }).await.unwrap(), 7);
        handle.cancel();
        assert!(handle.run(std::future::pending::<()>()).await.is_err());

        handle.rearm();
        let in_flight = InFlight::enter(&handle);
        handle.cancel();
        let started = Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(in_flight);
        });
        assert!(handle.run(std::future::pending::<()>()).await.is_err());
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn a_plan_run_finishes_the_tool_in_flight_then_stops_before_the_next() {
        let root = tempfile::tempdir().unwrap();
        let tools = root.path().join("tools");
        let plans = root.path().join("plans");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&plans).unwrap();
        let first_done = root.path().join("first_done");
        let second_started = root.path().join("second_started");
        let tool = |name: &str, script: String| {
            std::fs::write(
                tools.join(format!("{name}.yaml")),
                format!(
                    "version: 1\nname: {name}\ndescription: Test step.\nkind: exec\ncommand: bash\nargs: [\"-c\", \"{script}\"]\ninput_schema: {{type: object}}\n"
                ),
            )
            .unwrap();
        };
        tool(
            "first",
            format!("sleep 1; touch {}; echo '{{}}'", first_done.display()),
        );
        tool(
            "second",
            format!("touch {}; echo '{{}}'", second_started.display()),
        );
        std::fs::write(
            plans.join("two_steps.yaml"),
            "version: 2\nidentifier: two_steps\nname: Two steps\ndescription: Two tools in a row.\nsteps:\n- id: a\n  tool_name: user__first\n  input: {}\n- id: b\n  tool_name: user__second\n  input: {}\nfinish:\n  output:\n    done: '{{b}}'\n",
        )
        .unwrap();
        let mut config = dive_config::Config::default();
        config.tools.paths = vec![tools];
        config.plans.paths = vec![plans];
        let options = RuntimeOptions {
            storage_backend: Some("memory".to_string()),
            capture_content: false,
        };
        let runtime = Runtime::with_packs(config, InstalledPacks::default(), options).unwrap();
        let store = runtime.store().unwrap();
        let handle = CancelHandle::new();
        let hooks = PipelineHooks {
            cancel: Some(handle.clone()),
            ..Default::default()
        };
        let pipeline = runtime
            .pipeline_with(&store, Arc::new(NullSink), hooks)
            .await
            .unwrap();
        let canceller = handle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            canceller.cancel();
        });
        let started = Instant::now();
        let outcome = handle
            .run(pipeline.call_plan("two_steps", serde_json::json!({})))
            .await;
        let elapsed = started.elapsed();
        match outcome {
            Err(Cancelled) => {}
            Ok(call) => assert!(call.aborted, "the plan should stop: {:?}", call.result),
        }
        assert!(first_done.exists(), "the tool in flight should finish");
        assert!(!second_started.exists(), "the next tool should not start");
        assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }
}
