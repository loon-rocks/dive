use crate::store::EntryUsage;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

tokio::task_local! {
    static FRAME: Frame;
}

static FRAMES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Call {
    Model {
        role: String,
        site: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        decision: bool,
        ms: u64,
        usage: EntryUsage,
    },
    Tool {
        name: String,
        ms: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<Call>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Tool,
    Step,
}

#[derive(Clone)]
pub struct Frame(Arc<Node>);

struct Node {
    id: u64,
    kind: FrameKind,
    parent: Option<Frame>,
    calls: Mutex<Vec<Call>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub id: u64,
    pub kind: FrameKind,
}

impl Frame {
    fn new(kind: FrameKind) -> Self {
        Frame(Arc::new(Node {
            id: FRAMES.fetch_add(1, Ordering::Relaxed) + 1,
            kind,
            parent: FRAME.try_with(Clone::clone).ok(),
            calls: Mutex::new(Vec::new()),
        }))
    }

    fn push(&self, call: Call) {
        self.0.calls.lock().unwrap().push(call);
    }

    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.0.calls.lock().unwrap())
    }
}

pub fn lineage() -> Vec<FrameInfo> {
    let mut out = Vec::new();
    let mut frame = FRAME.try_with(Clone::clone).ok();
    while let Some(current) = frame {
        out.push(FrameInfo {
            id: current.0.id,
            kind: current.0.kind,
        });
        frame = current.0.parent.clone();
    }
    out
}

pub fn record(call: Call) {
    let _ = FRAME.try_with(|frame| frame.push(call));
}

pub struct Traced {
    pub elapsed: Duration,
    pub calls: Vec<Call>,
}

pub async fn tool<F: Future>(kind: FrameKind, future: F) -> (F::Output, Traced) {
    let frame = Frame::new(kind);
    let started = Instant::now();
    let output = FRAME.scope(frame.clone(), future).await;
    let traced = Traced {
        elapsed: started.elapsed(),
        calls: frame.take(),
    };
    (output, traced)
}

impl Traced {
    pub fn ms(&self) -> u64 {
        u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    pub fn finish(self, name: &str, is_error: bool) -> Vec<Call> {
        let calls = self.calls;
        let _ = FRAME.try_with(|parent| {
            parent.push(Call::Tool {
                name: name.to_string(),
                ms: u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX),
                is_error,
                calls: calls.clone(),
            })
        });
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(role: &str) -> Call {
        Call::Model {
            role: role.to_string(),
            site: role.to_string(),
            decision: false,
            ms: 5,
            usage: EntryUsage {
                model: "m".to_string(),
                input_tokens: 10,
                output_tokens: 2,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        }
    }

    #[tokio::test]
    async fn nested_and_concurrent_calls_land_in_their_own_frames() {
        let ((), outer) = tool(FrameKind::Tool, async {
            record(model("drafter"));
            let inner = |name: &'static str| async move {
                let ((), traced) = tool(FrameKind::Step, async {
                    tokio::task::yield_now().await;
                    record(model(name));
                })
                .await;
                traced.finish(name, false);
            };
            futures::join!(inner("a"), inner("b"));
        })
        .await;
        let calls = outer.finish("agent__tool_drafter", false);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], model("drafter"));
        for (call, name) in calls[1..].iter().zip(["a", "b"]) {
            match call {
                Call::Tool { name: n, calls, .. } => {
                    assert_eq!(n, name);
                    assert_eq!(calls, &vec![model(name)]);
                }
                other => panic!("expected a tool call, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn outside_any_frame_nothing_is_kept() {
        record(model("chat"));
        assert!(lineage().is_empty());
        let ((), traced) = tool(FrameKind::Tool, async {
            assert_eq!(lineage().len(), 1);
            assert_eq!(lineage()[0].kind, FrameKind::Tool);
        })
        .await;
        assert!(traced.finish("x", false).is_empty());
    }

    #[test]
    fn calls_round_trip_without_empty_fields() {
        let call = Call::Tool {
            name: "plan__digest".to_string(),
            ms: 40,
            is_error: false,
            calls: vec![model("judge")],
        };
        let text = serde_json::to_string(&call).unwrap();
        assert!(!text.contains("is_error"), "{text}");
        assert!(!text.contains("decision"), "{text}");
        assert_eq!(serde_json::from_str::<Call>(&text).unwrap(), call);
    }
}
