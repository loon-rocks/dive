use dive_core::run::Identity;
use dive_core::store::{
    CancelOutcome, Entry, EntryBody, FenceError, Lease, NewEntry, NewSession, Page, RunEnded,
    RunStarted, RunStatus, SessionFilter, SessionKind, Span, StartError, StartRun, Store,
    LEASE_EXPIRES_AFTER,
};
use dive_llm::types::ChatMessage;
use dive_store::Clock;
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

pub struct Time(Arc<AtomicI64>);

impl Time {
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(1_767_225_600_000)))
    }

    pub fn clock(&self) -> Clock {
        let now = Arc::clone(&self.0);
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn past_expiry(&self) {
        self.advance(LEASE_EXPIRES_AFTER.as_millis() as i64 + 1);
    }
}

pub struct Handles {
    pub name: &'static str,
    pub one: Arc<dyn Store>,
    pub two: Arc<dyn Store>,
    pub time: Time,
}

pub fn ada() -> Identity {
    Identity {
        user: "ada".into(),
        tenant: "local".into(),
        deployment: "cli".into(),
        environment: "local".into(),
    }
}

pub fn start(run: &str, session: &str, holder: &str) -> StartRun {
    StartRun {
        run_id: run.to_string(),
        session: session.to_string(),
        kind: SessionKind::Chat,
        holder: holder.to_string(),
        token_expires_at: None,
        started: RunStarted {
            input: json!("hi"),
            identity: ada(),
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
            plan: None,
            synthetic: false,
        },
    }
}

pub fn said(lease: &mut Lease, text: &str) -> Entry {
    let entry = NewEntry::message("user", ChatMessage::user(text)).into_entry(
        lease.next_seq,
        &lease.run,
        0,
    );
    lease.next_seq += 1;
    entry
}

async fn chat(store: &dyn Store) -> String {
    store
        .create_session(NewSession::chat("t", "chat", &ada()))
        .await
        .unwrap()
        .id
}

fn kinds(entries: &[Entry]) -> Vec<&'static str> {
    entries
        .iter()
        .map(|entry| match &entry.body {
            EntryBody::RunStarted(_) => "run_started",
            EntryBody::RunEnded(_) => "run_ended",
            EntryBody::Message { .. } => "message",
            EntryBody::CancelRequested { .. } => "cancel_requested",
            _ => "other",
        })
        .collect()
}

fn ended(entries: &[Entry]) -> Option<&RunEnded> {
    entries.iter().rev().find_map(|entry| match &entry.body {
        EntryBody::RunEnded(ended) => Some(ended),
        _ => None,
    })
}

pub async fn sessions_are_created_listed_filtered_paged_and_deleted(h: &Handles) {
    let name = h.name;
    let store = h.one.as_ref();
    let first = store
        .create_session(NewSession::chat("first", "front", &ada()).in_project(Some("/work")))
        .await
        .unwrap();
    assert_eq!(
        (
            first.tenant.as_str(),
            first.owner.as_str(),
            first.deployment.as_str(),
            first.environment.as_str()
        ),
        ("local", "ada", "cli", "local"),
        "{name}"
    );
    assert_eq!(
        (first.agent.as_str(), first.active_agent.as_str()),
        ("front", "front")
    );
    assert_eq!(first.active_run, None, "{name}");
    h.time.advance(5);
    let plan = store
        .create_session(NewSession::plan("digest", "daily_digest", &ada()))
        .await
        .unwrap();
    assert_eq!(plan.kind, SessionKind::Plan, "{name}");
    assert!(store
        .session_entries(&first.id, None)
        .await
        .unwrap()
        .is_empty());

    let all = store
        .list_sessions(&SessionFilter::default(), Page::ALL)
        .await
        .unwrap();
    let ids: Vec<&str> = all.sessions.iter().map(|meta| meta.id.as_str()).collect();
    assert_eq!(ids, [plan.id.as_str(), first.id.as_str()], "{name}");
    let one = store
        .list_sessions(&SessionFilter::default(), Page::first(1))
        .await
        .unwrap();
    assert_eq!((one.sessions.len(), one.more), (1, true), "{name}");
    let plans = SessionFilter {
        kind: Some(SessionKind::Plan),
        ..SessionFilter::default()
    };
    let listed = store.list_sessions(&plans, Page::ALL).await.unwrap();
    assert_eq!(listed.sessions, std::slice::from_ref(&plan), "{name}");
    let elsewhere = SessionFilter {
        tenant: Some("acme".into()),
        ..SessionFilter::default()
    };
    assert!(store
        .list_sessions(&elsewhere, Page::ALL)
        .await
        .unwrap()
        .sessions
        .is_empty());
    let in_work = SessionFilter {
        project: Some(Some("/work".into())),
        ..SessionFilter::default()
    };
    let listed = store.list_sessions(&in_work, Page::ALL).await.unwrap();
    assert_eq!(listed.sessions.len(), 1, "{name}");

    assert!(store.delete_session(&first.id).await.unwrap(), "{name}");
    assert!(!store.delete_session(&first.id).await.unwrap(), "{name}");
    assert!(store.get_session(&first.id).await.unwrap().is_none());
}

pub async fn two_concurrent_starts_on_one_session_get_one_lease_and_one_busy(h: &Handles) {
    let id = chat(h.one.as_ref()).await;
    let (a, b) = tokio::join!(
        h.one.start_run(start("run-a", &id, "one")),
        h.two.start_run(start("run-b", &id, "two"))
    );
    let (lease, busy, loser) = match (a, b) {
        (Ok(lease), Err(busy)) => (lease, busy, "run-b"),
        (Err(busy), Ok(lease)) => (lease, busy, "run-a"),
        other => panic!("{}: expected one lease and one busy, got {other:?}", h.name),
    };
    match busy {
        StartError::Busy { active_run } => assert_eq!(active_run, lease.run, "{}", h.name),
        other => panic!("{}: {other:?}", h.name),
    }
    assert!(h.one.get_run(loser).await.unwrap().is_none(), "{}", h.name);
    let meta = h.one.get_session(&id).await.unwrap().unwrap();
    assert_eq!(meta.active_run.as_deref(), Some(lease.run.as_str()));
    assert_eq!(meta.lease_generation, lease.generation);
    let entries = h.one.session_entries(&id, None).await.unwrap();
    assert_eq!(kinds(&entries), ["run_started"], "{}", h.name);
    assert_eq!(entries[0].seq, 1);
    assert_eq!(lease.next_seq, 2);
}

pub async fn a_reused_run_id_is_refused_naming_its_run_and_session(h: &Handles) {
    let first = chat(h.one.as_ref()).await;
    let second = chat(h.one.as_ref()).await;
    let lease = h
        .one
        .start_run(start("jti-1", &first, "one"))
        .await
        .unwrap();
    h.one
        .end_run(&lease, RunEnded::completed(None))
        .await
        .unwrap();
    match h.two.start_run(start("jti-1", &second, "two")).await {
        Err(StartError::Duplicate { run, session }) => {
            assert_eq!((run.as_str(), session.as_str()), ("jti-1", first.as_str()));
        }
        other => panic!("{}: {other:?}", h.name),
    }
    assert!(h
        .one
        .get_session(&second)
        .await
        .unwrap()
        .unwrap()
        .active_run
        .is_none());
}

pub async fn a_stale_generation_loses_its_lease_and_nothing_it_sent_lands(h: &Handles) {
    let id = chat(h.one.as_ref()).await;
    let mut lease = h.one.start_run(start("run-a", &id, "one")).await.unwrap();
    let mut stale = lease.clone();
    stale.generation -= 1;
    let ghost = said(&mut stale, "ghost");
    let lost = h.two.append(&stale, &[ghost]).await;
    assert!(matches!(lost, Err(FenceError::LeaseLost)), "{}", h.name);
    assert!(matches!(
        h.two.renew(&stale).await,
        Err(FenceError::LeaseLost)
    ));
    assert!(matches!(
        h.two.end_run(&stale, RunEnded::completed(None)).await,
        Err(FenceError::LeaseLost)
    ));
    let first = said(&mut lease, "one");
    let second = said(&mut lease, "two");
    h.one
        .append(&lease, &[first.clone(), second])
        .await
        .unwrap();
    let duplicate = h.one.append(&lease, &[first]).await;
    assert!(matches!(duplicate, Err(FenceError::Store(_))), "{}", h.name);
    let entries = h.one.session_entries(&id, None).await.unwrap();
    assert_eq!(kinds(&entries), ["run_started", "message", "message"]);
    let seqs: Vec<u64> = entries.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, [1, 2, 3], "{}", h.name);
    assert_eq!(
        h.one.session_entries(&id, Some(2)).await.unwrap().len(),
        1,
        "{}",
        h.name
    );
}

pub async fn ending_a_run_closes_it_and_frees_the_session(h: &Handles) {
    let id = chat(h.one.as_ref()).await;
    let mut lease = h.one.start_run(start("run-a", &id, "one")).await.unwrap();
    let busy = h.two.start_run(start("run-b", &id, "two")).await;
    assert!(matches!(busy, Err(StartError::Busy { .. })), "{}", h.name);
    h.one.set_session_agent(&lease, "back").await.unwrap();
    let entry = said(&mut lease, "hello");
    h.one.append(&lease, &[entry]).await.unwrap();
    h.time.advance(10);
    h.one
        .end_run(&lease, RunEnded::completed(Some(json!("done"))))
        .await
        .unwrap();
    let run = h.one.get_run("run-a").await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{}", h.name);
    assert_eq!((run.first_seq, run.last_seq), (1, Some(3)), "{}", h.name);
    assert!(run.ended_at.is_some());
    assert_eq!(run.owner, "ada");
    let meta = h.one.get_session(&id).await.unwrap().unwrap();
    assert_eq!(meta.active_run, None, "{}", h.name);
    assert_eq!(meta.lease_holder, None, "{}", h.name);
    assert_eq!(meta.active_agent, "back", "{}", h.name);
    assert_eq!(meta.message_count, 1, "{}", h.name);
    assert!(matches!(
        h.one.set_session_agent(&lease, "front").await,
        Err(FenceError::LeaseLost)
    ));
    let entries = h.one.session_entries(&id, None).await.unwrap();
    assert_eq!(kinds(&entries), ["run_started", "message", "run_ended"]);
    assert!(entries.iter().all(|entry| entry.run_id == "run-a"));
    h.two.start_run(start("run-b", &id, "two")).await.unwrap();
}

pub async fn a_held_run_sees_a_cancel_on_its_next_renewal(h: &Handles) {
    let id = chat(h.one.as_ref()).await;
    let lease = h.one.start_run(start("run-a", &id, "one")).await.unwrap();
    let renewal = h.one.renew(&lease).await.unwrap();
    assert!(!renewal.cancel_requested, "{}", h.name);
    h.time.advance(1000);
    let renewed = h.one.renew(&lease).await.unwrap();
    assert!(renewed.expires_at > renewal.expires_at, "{}", h.name);
    assert_eq!(
        h.two.request_cancel("run-a", "ada").await.unwrap(),
        CancelOutcome::Signalled,
        "{}",
        h.name
    );
    assert!(h.one.renew(&lease).await.unwrap().cancel_requested);
    let entries = h.one.session_entries(&id, None).await.unwrap();
    assert_eq!(kinds(&entries), ["run_started"], "{}", h.name);
}

pub async fn a_cancel_of_a_run_no_one_holds_ends_it(h: &Handles) {
    let id = chat(h.one.as_ref()).await;
    let lease = h.one.start_run(start("run-a", &id, "one")).await.unwrap();
    h.time.past_expiry();
    assert_eq!(
        h.two.request_cancel("run-a", "ada").await.unwrap(),
        CancelOutcome::Ended,
        "{}",
        h.name
    );
    let entries = h.one.session_entries(&id, None).await.unwrap();
    assert_eq!(
        kinds(&entries),
        ["run_started", "cancel_requested", "run_ended"]
    );
    let seqs: Vec<u64> = entries.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, [1, 2, 3], "{}", h.name);
    assert_eq!(ended(&entries).unwrap().status, RunStatus::Cancelled);
    assert_eq!(
        h.one.get_run("run-a").await.unwrap().unwrap().status,
        RunStatus::Cancelled
    );
    assert!(h
        .one
        .get_session(&id)
        .await
        .unwrap()
        .unwrap()
        .active_run
        .is_none());
    assert!(matches!(
        h.one.renew(&lease).await,
        Err(FenceError::LeaseLost)
    ));
    assert_eq!(
        h.two.request_cancel("run-a", "ada").await.unwrap(),
        CancelOutcome::NotRunning
    );
    assert!(h.one.request_cancel("missing", "ada").await.is_err());
}

pub async fn the_reaper_fails_chat_runs_and_interrupts_plan_runs_once(h: &Handles) {
    let chat_id = chat(h.one.as_ref()).await;
    let plan_id = h
        .one
        .create_session(NewSession::plan("p", "digest", &ada()))
        .await
        .unwrap()
        .id;
    let chat_lease = h
        .one
        .start_run(start("chat-run", &chat_id, "one"))
        .await
        .unwrap();
    let plan_lease = h
        .one
        .start_run(StartRun {
            kind: SessionKind::Plan,
            ..start("plan-run", &plan_id, "one")
        })
        .await
        .unwrap();
    assert!(
        h.two.reap_expired(10).await.unwrap().is_empty(),
        "{}",
        h.name
    );
    h.time.past_expiry();
    let (a, b) = tokio::join!(h.one.reap_expired(10), h.two.reap_expired(10));
    let mut reaped: Vec<_> = a.unwrap().into_iter().chain(b.unwrap()).collect();
    reaped.sort_by(|x, y| x.run.cmp(&y.run));
    let summary: Vec<(&str, RunStatus)> = reaped
        .iter()
        .map(|done| (done.run.as_str(), done.status))
        .collect();
    assert_eq!(
        summary,
        [
            ("chat-run", RunStatus::Failed),
            ("plan-run", RunStatus::Interrupted)
        ],
        "{}",
        h.name
    );
    assert!(h.one.reap_expired(10).await.unwrap().is_empty());
    let chat_meta = h.one.get_session(&chat_id).await.unwrap().unwrap();
    assert_eq!(chat_meta.active_run, None, "{}", h.name);
    let plan_meta = h.one.get_session(&plan_id).await.unwrap().unwrap();
    assert_eq!(
        plan_meta.active_run.as_deref(),
        Some("plan-run"),
        "{}",
        h.name
    );
    assert_eq!(plan_meta.lease_holder, None, "{}", h.name);
    let entries = h.one.session_entries(&chat_id, None).await.unwrap();
    assert_eq!(kinds(&entries), ["run_started", "run_ended"], "{}", h.name);
    assert_eq!(ended(&entries).unwrap().status, RunStatus::Failed);
    let entries = h.one.session_entries(&plan_id, None).await.unwrap();
    assert_eq!(ended(&entries).unwrap().status, RunStatus::Interrupted);
    for lease in [&chat_lease, &plan_lease] {
        assert!(matches!(
            h.one.append(lease, &[]).await,
            Ok(()) | Err(FenceError::LeaseLost)
        ));
        assert!(matches!(
            h.one.renew(lease).await,
            Err(FenceError::LeaseLost)
        ));
    }
    assert!(matches!(
        h.one.start_run(start("again", &plan_id, "one")).await,
        Err(StartError::Busy { .. })
    ));
    h.one
        .start_run(start("again", &chat_id, "one"))
        .await
        .unwrap();
}

pub async fn spans_and_attachments_live_on_the_session(h: &Handles) {
    let id = chat(h.one.as_ref()).await;
    let span: Span = serde_json::from_value(json!({
        "trace_id": "t", "span_id": "s1", "kind": "turn", "name": "chat",
        "start_time_unix_nano": 1
    }))
    .unwrap();
    h.one
        .append_session_spans(&id, std::slice::from_ref(&span))
        .await
        .unwrap();
    assert_eq!(
        h.two.session_spans(&id).await.unwrap().len(),
        1,
        "{}",
        h.name
    );
    let attachment = dive_core::store::Attachment::new("network").with("state", "open");
    h.one
        .set_session_attachments(&id, std::slice::from_ref(&attachment))
        .await
        .unwrap();
    assert_eq!(
        h.two.get_session(&id).await.unwrap().unwrap().attachments,
        [attachment]
    );
    assert!(h.one.set_session_attachments("missing", &[]).await.is_err());
}
