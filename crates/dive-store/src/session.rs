use dive_core::store::{
    cancel_entry, ended_entry, lease_expiry, messages_in, reaped_ending, CancelOutcome, Entry,
    FenceError, Lease, Reaped, Renewal, RunEnded, RunMeta, RunStatus, SessionMeta, StartError,
    StartRun,
};
use std::sync::Arc;

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn wall_clock() -> Clock {
    Arc::new(|| chrono::Utc::now().timestamp_millis())
}

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub struct Started {
    pub run: RunMeta,
    pub entry: Entry,
    pub lease: Lease,
}

pub fn start(
    session: &mut SessionMeta,
    last_seq: u64,
    start: &StartRun,
    now: i64,
) -> Result<Started, StartError> {
    if let Some(active_run) = &session.active_run {
        return Err(StartError::Busy {
            active_run: active_run.clone(),
        });
    }
    let seq = last_seq + 1;
    session.active_run = Some(start.run_id.clone());
    session.lease_generation += 1;
    session.lease_holder = Some(start.holder.clone());
    session.lease_expires_at = Some(lease_expiry(now));
    session.updated_at = session.updated_at.max(now);
    Ok(Started {
        run: start.run_meta(seq, now),
        entry: start.first_entry(seq, now),
        lease: start.lease(session.lease_generation, seq),
    })
}

pub fn fence(session: &SessionMeta, lease: &Lease) -> Result<(), FenceError> {
    if session.fences(lease) {
        Ok(())
    } else {
        Err(FenceError::LeaseLost)
    }
}

pub fn renew(
    session: &mut SessionMeta,
    run: &RunMeta,
    lease: &Lease,
    now: i64,
) -> Result<Renewal, FenceError> {
    fence(session, lease)?;
    let expires_at = lease_expiry(now);
    session.lease_expires_at = Some(expires_at);
    Ok(Renewal {
        cancel_requested: run.cancel_requested,
        expires_at,
    })
}

pub fn finish(
    session: &mut SessionMeta,
    run: &mut RunMeta,
    last_seq: u64,
    messages: u64,
    ended: RunEnded,
    now: i64,
) -> Entry {
    let seq = last_seq + 1;
    let keeps_claim = ended.status == RunStatus::Interrupted;
    run.status = ended.status;
    run.last_seq = Some(seq);
    run.ended_at = Some(now);
    session.lease_holder = None;
    session.lease_expires_at = None;
    if keeps_claim {
        session.lease_generation += 1;
    } else {
        session.active_run = None;
    }
    session.message_count += messages;
    session.updated_at = session.updated_at.max(now);
    ended_entry(&run.id, seq, now, ended)
}

pub enum Cancel {
    Flag,
    End(Vec<Entry>),
    NotRunning,
}

impl Cancel {
    pub fn outcome(&self) -> CancelOutcome {
        match self {
            Self::Flag => CancelOutcome::Signalled,
            Self::End(_) => CancelOutcome::Ended,
            Self::NotRunning => CancelOutcome::NotRunning,
        }
    }
}

pub fn cancel(
    session: &mut SessionMeta,
    run: &mut RunMeta,
    last_seq: u64,
    messages: u64,
    by: &str,
    now: i64,
) -> Cancel {
    let claimed = session.active_run.as_deref() == Some(run.id.as_str());
    if !claimed || !matches!(run.status, RunStatus::Running | RunStatus::Interrupted) {
        return Cancel::NotRunning;
    }
    run.cancel_requested = true;
    if session.held(now) {
        return Cancel::Flag;
    }
    let requested = cancel_entry(&run.id, last_seq + 1, now, by);
    let ended = finish(
        session,
        run,
        last_seq + 1,
        messages,
        RunEnded::new(RunStatus::Cancelled),
        now,
    );
    session.active_run = None;
    Cancel::End(vec![requested, ended])
}

pub fn reap(
    session: &mut SessionMeta,
    run: &mut RunMeta,
    last_seq: u64,
    messages: u64,
    now: i64,
) -> Option<(Entry, Reaped)> {
    if !session.lease_expired(now) || session.active_run.as_deref() != Some(run.id.as_str()) {
        return None;
    }
    let ended = reaped_ending(run.kind);
    let status = ended.status;
    let entry = finish(session, run, last_seq, messages, ended, now);
    Some((
        entry,
        Reaped {
            run: run.id.clone(),
            session: session.id.clone(),
            kind: run.kind,
            status,
        },
    ))
}

pub fn messages_of(entries: &[Entry], run: &str) -> u64 {
    messages_in(entries.iter().filter(|entry| entry.run_id == run))
}
