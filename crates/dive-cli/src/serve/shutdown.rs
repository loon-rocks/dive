use super::AppState;
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub async fn signal() {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(term) => term,
            Err(error) => {
                tracing::warn!(error = %error, "can't listen for SIGTERM; only Ctrl-C stops the server");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>, grace: Duration) {
    serve_until(listener, state, grace, signal()).await
}

pub async fn serve_until(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    grace: Duration,
    stop: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let stopping = CancellationToken::new();
    let reaper = tokio::spawn(reap_every(
        Arc::clone(&state),
        dive_core::store::LEASE_RENEW_EVERY,
    ));
    let app = super::router(Arc::clone(&state));
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(stopping.clone().cancelled_owned())
        .into_future();
    let mut serving = tokio::spawn(server);
    tokio::select! {
        result = &mut serving => {
            reaper.abort();
            if let Ok(Err(error)) = result {
                tracing::error!(error = %error, "the server stopped");
            }
            return;
        }
        _ = stop => {}
    }
    eprintln!(
        "dive serve stopping; runs still running: {}",
        state.limits.running()
    );
    state.drain();
    stopping.cancel();
    let window = FORCE_WINDOW.min(grace / 2);
    let drained = tokio::time::timeout(grace.saturating_sub(window), async {
        state.wait_for_runs().await;
        let _ = (&mut serving).await;
        state.wait_for_runs().await;
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            grace_secs = grace.as_secs(),
            "runs were still going near the end of the grace period; ending them as interrupted"
        );
        state.force();
        if tokio::time::timeout(window, state.wait_for_runs())
            .await
            .is_err()
        {
            tracing::warn!(
                "some runs didn't end; the next dive serve will end them as interrupted"
            );
        }
        serving.abort();
    }
    reaper.abort();
}

const FORCE_WINDOW: Duration = Duration::from_secs(2);

async fn reap_every(state: Arc<AppState>, every: Duration) {
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        state.reap().await;
    }
}
