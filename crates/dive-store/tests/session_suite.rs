#[path = "common/suite.rs"]
mod suite;

use dive_store::{FileStore, MemoryStore};
use std::sync::Arc;
use suite::{Handles, Time};

fn memory() -> Handles {
    let time = Time::new();
    let store: Arc<dyn dive_core::store::Store> = Arc::new(MemoryStore::with_clock(time.clock()));
    Handles {
        name: "memory",
        one: store.clone(),
        two: store,
        time,
    }
}

fn file(dir: &tempfile::TempDir) -> Handles {
    let time = Time::new();
    Handles {
        name: "file",
        one: Arc::new(FileStore::open_with_clock(dir.path(), time.clock()).unwrap()),
        two: Arc::new(FileStore::open_with_clock(dir.path(), time.clock()).unwrap()),
        time,
    }
}

macro_rules! on_every_backend {
    ($($case:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread")]
            async fn $case() {
                suite::$case(&memory()).await;
                let dir = tempfile::tempdir().unwrap();
                suite::$case(&file(&dir)).await;
            }
        )*
    };
}

on_every_backend!(
    sessions_are_created_listed_filtered_paged_and_deleted,
    two_concurrent_starts_on_one_session_get_one_lease_and_one_busy,
    a_reused_run_id_is_refused_naming_its_run_and_session,
    a_stale_generation_loses_its_lease_and_nothing_it_sent_lands,
    ending_a_run_closes_it_and_frees_the_session,
    a_held_run_sees_a_cancel_on_its_next_renewal,
    a_cancel_of_a_run_no_one_holds_ends_it,
    the_reaper_fails_chat_runs_and_interrupts_plan_runs_once,
    spans_and_attachments_live_on_the_session,
);
