use crate::store::{Entry, EntryBody, NewEntry, RunStatus};
use dive_llm::types::ChatMessage;
use serde_json::Value;
use std::collections::HashSet;

pub trait Logged {
    fn author(&self) -> &str;
    fn body(&self) -> &EntryBody;
    fn path(&self) -> &str;
}

impl Logged for Entry {
    fn author(&self) -> &str {
        &self.author
    }
    fn body(&self) -> &EntryBody {
        &self.body
    }
    fn path(&self) -> &str {
        &self.path
    }
}

impl Logged for NewEntry {
    fn author(&self) -> &str {
        &self.author
    }
    fn body(&self) -> &EntryBody {
        &self.body
    }
    fn path(&self) -> &str {
        &self.path
    }
}

pub fn completed_runs(entries: &[Entry]) -> HashSet<&str> {
    entries
        .iter()
        .filter_map(|entry| match &entry.body {
            EntryBody::RunEnded(ended) if ended.status == RunStatus::Completed => {
                Some(entry.run_id.as_str())
            }
            _ => None,
        })
        .collect()
}

pub fn context_entries(entries: &[Entry], current_run: Option<&str>) -> Vec<NewEntry> {
    let completed = completed_runs(entries);
    let counts = |entry: &Entry| {
        completed.contains(entry.run_id.as_str()) || Some(entry.run_id.as_str()) == current_run
    };
    let kept: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.path.is_empty() && entry.body.is_conversation() && counts(entry))
        .collect();
    let from = kept
        .iter()
        .rposition(|entry| matches!(entry.body, EntryBody::Compaction { .. }))
        .unwrap_or(0);
    let mut out: Vec<NewEntry> = kept[from..]
        .iter()
        .map(|entry| NewEntry::from((*entry).clone()))
        .collect();
    order_tool_results(&mut out);
    out
}

pub fn run_entries(entries: &[Entry], run: &str) -> Vec<NewEntry> {
    entries
        .iter()
        .filter(|entry| entry.run_id == run)
        .cloned()
        .map(NewEntry::from)
        .collect()
}

pub fn order_tool_results(entries: &mut [NewEntry]) {
    let mut at = 0;
    while at < entries.len() {
        let calls: Vec<String> = match &entries[at].body {
            EntryBody::Message {
                message: ChatMessage::Assistant { tool_calls, .. },
            } if !tool_calls.is_empty() && entries[at].path.is_empty() => {
                tool_calls.iter().map(|call| call.id.clone()).collect()
            }
            _ => {
                at += 1;
                continue;
            }
        };
        let mut slots = Vec::new();
        let mut next = at + 1;
        while let Some(entry) = entries.get(next) {
            if result_id(entry).is_some() && entry.path.is_empty() {
                slots.push(next);
            } else if entry.path.is_empty() && matches!(entry.body, EntryBody::Message { .. }) {
                break;
            }
            next += 1;
        }
        let index = |entry: &NewEntry| {
            result_id(entry)
                .and_then(|id| calls.iter().position(|call| call == id))
                .unwrap_or(usize::MAX)
        };
        let mut results: Vec<NewEntry> = slots.iter().map(|&slot| entries[slot].clone()).collect();
        results.sort_by_key(index);
        for (slot, result) in slots.into_iter().zip(results) {
            entries[slot] = result;
        }
        at = next;
    }
}

fn result_id(entry: &NewEntry) -> Option<&str> {
    match &entry.body {
        EntryBody::Message {
            message: ChatMessage::ToolResult { tool_call_id, .. },
        } => Some(tool_call_id),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct SubagentCall<'a> {
    pub caller: &'a str,
    pub agent: &'a str,
    pub input: &'a Value,
    pub messages: Vec<&'a ChatMessage>,
    pub output: Option<&'a Value>,
    pub final_: bool,
}

#[derive(Debug)]
pub enum Folded<'a, T> {
    Entry(&'a T),
    Subagent(SubagentCall<'a>),
}

pub fn fold<T: Logged>(entries: &[T]) -> Vec<Folded<'_, T>> {
    let mut out = Vec::new();
    let mut open: Option<SubagentCall<'_>> = None;
    for entry in entries {
        match entry.body() {
            EntryBody::AgentStarted {
                agent,
                caller,
                input,
            } => {
                if let Some(call) = open.take() {
                    out.push(Folded::Subagent(call));
                }
                open = Some(SubagentCall {
                    caller,
                    agent,
                    input,
                    messages: Vec::new(),
                    output: None,
                    final_: false,
                });
            }
            EntryBody::AgentFinished { output, final_, .. } => match open.take() {
                Some(mut call) => {
                    call.output = Some(output);
                    call.final_ = *final_;
                    out.push(Folded::Subagent(call));
                }
                None => out.push(Folded::Entry(entry)),
            },
            EntryBody::Message { message } if !entry.path().is_empty() => match &mut open {
                Some(call) => call.messages.push(message),
                None => out.push(Folded::Entry(entry)),
            },
            _ => {
                if let Some(call) = open.take() {
                    out.push(Folded::Subagent(call));
                }
                out.push(Folded::Entry(entry));
            }
        }
    }
    if let Some(call) = open {
        out.push(Folded::Subagent(call));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewEntry, RunEnded, RUN_AUTHOR};
    use dive_llm::types::ToolCall;
    use serde_json::json;

    fn log(run: &str, seq: &mut u64, entry: NewEntry) -> Entry {
        *seq += 1;
        entry.into_entry(*seq, run, 0)
    }

    fn said(author: &str, text: &str) -> NewEntry {
        NewEntry::message(author, ChatMessage::user(text))
    }

    fn ended(status: RunStatus) -> NewEntry {
        NewEntry::new(RUN_AUTHOR, EntryBody::RunEnded(RunEnded::new(status)))
    }

    fn texts(entries: &[NewEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|entry| match &entry.body {
                EntryBody::Message {
                    message: ChatMessage::User { content, .. },
                } => Some(content.clone()),
                EntryBody::Message {
                    message: ChatMessage::ToolResult { tool_call_id, .. },
                } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_failed_run_never_reaches_the_context_and_the_current_one_does() {
        let mut seq = 0;
        let entries = vec![
            log("one", &mut seq, said("user", "first")),
            log("one", &mut seq, ended(RunStatus::Completed)),
            log("two", &mut seq, said("user", "second")),
            log("two", &mut seq, ended(RunStatus::Failed)),
            log("three", &mut seq, said("user", "cancelled")),
            log("three", &mut seq, ended(RunStatus::Cancelled)),
            log("four", &mut seq, said("user", "third")),
        ];
        assert_eq!(
            texts(&context_entries(&entries, Some("four"))),
            ["first", "third"]
        );
        assert_eq!(texts(&context_entries(&entries, None)), ["first"]);
    }

    #[test]
    fn the_context_starts_at_the_last_compaction_and_skips_subagent_messages() {
        let mut seq = 0;
        let entries = vec![
            log("one", &mut seq, said("user", "old")),
            log(
                "one",
                &mut seq,
                NewEntry::new(
                    "chat",
                    EntryBody::Compaction {
                        summary: "old talk".into(),
                    },
                ),
            ),
            log("one", &mut seq, said("user", "new")),
            log(
                "one",
                &mut seq,
                said("helper", "inside").at_path("agent:helper", Some(2)),
            ),
            log("one", &mut seq, ended(RunStatus::Completed)),
        ];
        let context = context_entries(&entries, None);
        assert!(matches!(context[0].body, EntryBody::Compaction { .. }));
        assert_eq!(texts(&context), ["new"]);
    }

    #[test]
    fn tool_results_follow_the_order_of_their_calls() {
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "t".into(),
            arguments: json!({}),
        };
        let result = |id: &str| {
            NewEntry::message(
                "chat",
                ChatMessage::ToolResult {
                    tool_call_id: id.into(),
                    content: json!({}),
                    is_error: false,
                },
            )
        };
        let mut seq = 0;
        let entries = vec![
            log(
                "one",
                &mut seq,
                NewEntry::message(
                    "chat",
                    ChatMessage::Assistant {
                        content: None,
                        thinking: Vec::new(),
                        tool_calls: vec![call("a"), call("b"), call("c")],
                    },
                ),
            ),
            log("one", &mut seq, result("c")),
            log("one", &mut seq, result("a")),
            log("one", &mut seq, result("b")),
            log("one", &mut seq, ended(RunStatus::Completed)),
        ];
        assert_eq!(texts(&context_entries(&entries, None)), ["a", "b", "c"]);
    }

    #[test]
    fn a_subagent_block_folds_into_one_call() {
        let entries = vec![
            NewEntry::new(
                "front",
                EntryBody::AgentStarted {
                    agent: "helper".into(),
                    caller: "front".into(),
                    input: json!({"q": 1}),
                },
            ),
            said("helper", "inside").at_path("agent:helper", Some(1)),
            NewEntry::new(
                "front",
                EntryBody::AgentFinished {
                    agent: "helper".into(),
                    output: json!("done"),
                    final_: true,
                },
            )
            .at_path("", Some(1)),
            said("user", "after"),
        ];
        let folded = fold(&entries);
        assert_eq!(folded.len(), 2);
        let Folded::Subagent(call) = &folded[0] else {
            panic!("{folded:?}");
        };
        assert_eq!(
            (call.agent, call.caller, call.messages.len(), call.final_),
            ("helper", "front", 1, true)
        );
        assert_eq!(call.output, Some(&json!("done")));
    }
}
