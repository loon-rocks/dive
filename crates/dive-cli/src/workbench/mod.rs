//! `dive workbench` — the interactive plan IDE: a dual-pane TUI with the
//! chat agent on the left and the plan workspace (structure, context, runs)
//! on the right. See docs/getting-started/workbench.mdx.

mod answer;
mod app;
mod edit;
mod editor;
mod form;
mod plan_ws;
#[cfg(test)]
mod screenshot;
#[cfg(test)]
mod shots;
mod ui;

pub(crate) use dive_runtime::workbench::{agents, artifact, event, runner};

use crate::cli::WorkbenchCommand;
use anyhow::{bail, Result};
use app::{App, Msg};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use dive_runtime::runtime::Runtime;
use dive_runtime::workbench::{Workbench, WorkbenchEvent};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{IsTerminal, Stdout};
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn run(command: Option<WorkbenchCommand>, verbosity: u8) -> Result<()> {
    let start = match command {
        None => agents::Start::FrontDesk,
        Some(WorkbenchCommand::Plan { name_or_path }) => agents::Start::Plan(name_or_path),
        Some(WorkbenchCommand::Agent { name }) => agents::Start::Agent(name),
        Some(WorkbenchCommand::Tool { name }) => agents::Start::Tool(name),
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "the workbench needs an interactive terminal — \
             use `dive plan run` or `dive ask` for scripting"
        );
    }
    let runtime = crate::host::runtime()?;
    let log_path = init_debug_log(&runtime, verbosity);
    let result = run_workbench(&runtime, start, log_path).await;
    // MCP child processes must shut down before the tokio runtime drops.
    runtime.shutdown().await;
    result
}

/// Route tracing to a log file: the TUI owns the terminal, so stderr
/// output would scribble over it. Always on — the default filter keeps
/// the workbench's own instrumentation at debug and everything else at
/// warn; `-v` flags raise it and `DIVE_LOG` overrides it entirely.
/// The path resolves `DIVE_WORKBENCH_LOG` → `[workbench].log_path` →
/// `<data_dir>/workbench.log` (appended across sessions).
fn init_debug_log(runtime: &Runtime, verbosity: u8) -> Option<std::path::PathBuf> {
    let path = log_path(&runtime.config, dive_config::legacy::var_os("DIVE_WORKBENCH_LOG"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let filter = dive_config::legacy::var("DIVE_LOG")
        .and_then(|filter| tracing_subscriber::EnvFilter::try_new(filter).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(default_log_filter(verbosity)));
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .finish()
        .with(crate::telemetry::logs_layer())
        .try_init()
        .ok()?;
    tracing::info!(
        target: "workbench",
        "── session started (dive {}) ──",
        env!("CARGO_PKG_VERSION")
    );
    Some(path)
}

/// Env var → `[workbench].log_path` (tilde-expanded) → the data dir.
fn log_path(
    config: &dive_config::Config,
    env_override: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    if let Some(path) = env_override {
        return std::path::PathBuf::from(path);
    }
    match &config.workbench.log_path {
        Some(path) => dive_config::expand_tilde(path),
        None => dive_config::expand_tilde(&config.settings.data_dir).join("workbench.log"),
    }
}

/// `workbench` is the explicit target on every workbench log line, so the
/// instrumentation stays selectable regardless of module layout.
fn default_log_filter(verbosity: u8) -> &'static str {
    match verbosity {
        0 => "warn,workbench=debug",
        1 => "info,workbench=debug",
        2 => "debug,workbench=trace",
        _ => "trace",
    }
}

async fn run_workbench(
    runtime: &Runtime,
    start: agents::Start,
    log_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorkbenchEvent>();
    let context = Arc::new(Workbench::open(runtime, start, std::env::current_dir()?, tx).await?);

    let mut app = App::new(context.plan());
    app.agent = context.active_agent();
    app.log_path = log_path;
    let network = context.network();
    app.network = (!network.is_empty()).then_some(network);
    run_effect(app::Effect::LoadContext, &context);
    if app.ws.doc.is_some() {
        run_effect(app::Effect::Validate, &context);
    }

    let (mut terminal, enhanced_keys) = setup_terminal()?;
    app.enhanced_keys = enhanced_keys;
    let loop_result = event_loop(&mut terminal, &mut app, &mut rx, &context).await;
    restore_terminal(&mut terminal)?;
    loop_result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<WorkbenchEvent>,
    context: &Arc<Workbench>,
) -> Result<()> {
    let mut term_events = EventStream::new();
    // The animation heartbeat only fires while something is executing, so
    // an idle workbench draws nothing and paused states stay static.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        let msg = tokio::select! {
            maybe_event = term_events.next() => match maybe_event {
                Some(Ok(event)) => Msg::Term(event),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            },
            Some(event) = rx.recv() => Msg::Workbench(event),
            _ = ticker.tick(), if app.wants_tick() => Msg::Tick,
        };
        if let Some((level, line)) = msg.log_line() {
            match level {
                tracing::Level::TRACE => tracing::trace!(target: "workbench", "{line}"),
                _ => tracing::debug!(target: "workbench", "{line}"),
            }
        }
        let mode_before = app.mode.label();
        let status_before = app.status.clone();
        for effect in app::update(app, msg) {
            tracing::debug!(target: "workbench", "effect: {}", effect.label());
            run_effect(effect, context);
        }
        if app.mode.label() != mode_before {
            tracing::debug!(
                target: "workbench",
                "mode: {mode_before} → {}",
                app.mode.label()
            );
        }
        if app.status != status_before {
            tracing::trace!(target: "workbench", "status: {}", app.status);
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

fn run_effect(effect: app::Effect, context: &Arc<Workbench>) {
    let ctx = context.clone();
    match effect {
        app::Effect::RunAgentTurn { message } => {
            tokio::spawn(async move { ctx.send_message(message).await });
        }
        app::Effect::Agent { name } => ctx.select_agent(name),
        app::Effect::StartRun { gated, input } => {
            tokio::spawn(async move { ctx.start_run(gated, input).await });
        }
        app::Effect::Validate => ctx.validate(),
        app::Effect::LoadContext => {
            tokio::spawn(async move { ctx.load_context().await });
        }
        app::Effect::SyncDebug { breakpoints } => ctx.sync_debug(breakpoints),
        app::Effect::ApplyEdit { edit, commit } => ctx.apply_edit(commit, |doc| edit.apply(doc)),
        app::Effect::SavePlan => {
            let _ = ctx.save_plan();
        }
        app::Effect::SaveNetwork => {
            tokio::spawn(async move { ctx.save_network().await });
        }
        app::Effect::DiscardNetwork => {
            ctx.discard_network();
        }
        app::Effect::RestoreDraft => ctx.restore_draft(),
    }
}

// ── Terminal lifecycle ───────────────────────────────────────────────────

fn setup_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, bool)> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Bracketed paste: a multi-line paste arrives as one Event::Paste
    // instead of a stream of keypresses whose Enters would submit mid-paste.
    // Mouse capture routes clicks/scroll to us as Event::Mouse; native
    // text selection then needs the terminal's modifier (Shift/Option).
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        crossterm::execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    install_panic_hook();
    Ok((Terminal::new(CrosstermBackend::new(stdout))?, enhanced))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// A panic mid-draw must not leave the user's terminal in raw mode (or
/// with bracketed paste / mouse capture still on) with no visible error.
fn install_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                PopKeyboardEnhancementFlags,
                DisableMouseCapture,
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            original(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_resolves_env_then_config_then_data_dir() {
        let mut config = dive_config::Config::default();
        assert!(
            log_path(&config, None).ends_with("workbench.log"),
            "default lands in the data dir"
        );

        config.workbench.log_path = Some("~/logs/wb.log".into());
        let from_config = log_path(&config, None);
        assert!(from_config.ends_with("logs/wb.log"));
        assert!(
            !from_config.starts_with("~"),
            "config paths are tilde-expanded"
        );

        assert_eq!(
            log_path(&config, Some("/tmp/env.log".into())),
            std::path::PathBuf::from("/tmp/env.log"),
            "the env var beats the config"
        );
    }

    #[test]
    fn default_log_filter_scales_with_verbosity() {
        assert_eq!(default_log_filter(0), "warn,workbench=debug");
        assert_eq!(default_log_filter(1), "info,workbench=debug");
        assert_eq!(default_log_filter(2), "debug,workbench=trace");
        assert_eq!(default_log_filter(9), "trace");
    }
}
