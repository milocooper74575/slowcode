//! slowcode terminal user interface.
//!
//! The TUI renders committed kernel facts. It never drives the agent loop
//! directly and never turns a slash command into a prompt.

mod app;
mod approval;

use std::{io::stdout, sync::Arc, time::Duration};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use slowcode_app::{Runtime, RuntimeConfig, SessionChoice};
use slowcode_mcp::McpConfig;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    app::{App, AppEvent},
    approval::TuiApprovalProvider,
};

/// Startup options for one TUI run.
#[derive(Clone, Debug)]
pub struct TuiConfig {
    pub runtime: RuntimeConfig,
    pub session: SessionChoice,
    pub ask: bool,
}

/// TUI errors.
#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Runtime(#[from] slowcode_app::RuntimeError),
    #[error("terminal setup failed: {0}")]
    Terminal(String),
}

/// Run the TUI until the user quits.
pub async fn run(config: TuiConfig) -> Result<(), TuiError> {
    let mut terminal = setup_terminal()?;
    let result = run_inner(&mut terminal, config).await;
    let restore = restore_terminal(&mut terminal);
    result.and(restore)
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    config: TuiConfig,
) -> Result<(), TuiError> {
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppEvent>();

    let mut started = Runtime::start(config.runtime, config.session).await?;
    let approval: Arc<dyn slowcode_kernel::agent::ApprovalProvider> =
        TuiApprovalProvider::new(approval_tx);
    if config.ask {
        started.runtime.install_approval_provider(approval);
    } else {
        started.runtime.install_approval_provider_passive(approval);
    }

    let session_id = started.session_id.clone();
    let history = started.history;
    let workspace = started.runtime.workspace().to_path_buf();
    let model = started.runtime.model().to_owned();
    let role = started
        .runtime
        .role()
        .map(|role| role.as_str().to_owned())
        .unwrap_or_else(|| "driver".to_owned());
    let runtime = Arc::new(tokio::sync::Mutex::new(started.runtime));
    let mut events = started.events;
    let app_tx_for_mcp = app_tx.clone();
    let mut app = App::new(
        Arc::clone(&runtime),
        session_id,
        model,
        role,
        history,
        app_tx,
    );
    if McpConfig::load(&workspace)
        .map(|config| !config.servers.is_empty())
        .unwrap_or(false)
    {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            let result = {
                let mut runtime = runtime.lock().await;
                runtime
                    .install_mcp_tools()
                    .await
                    .map(|report| report.summary())
                    .map_err(|error| error.to_string())
            };
            let _ = app_tx_for_mcp.send(AppEvent::McpRefreshed(result));
        });
    }
    let mut event_stream = EventStream::new();

    loop {
        terminal.draw(|frame| app.draw(frame))?;

        tokio::select! {
            event = events.recv() => {
                if let Some(event) = event {
                    app.handle_ui_event(event);
                }
            }
            approval = approval_rx.recv() => {
                if let Some(message) = approval {
                    app.handle_approval(message);
                }
            }
            app_event = app_rx.recv() => {
                if let Some(app_event) = app_event {
                    app.handle_app_event(app_event);
                }
            }
            event = event_stream.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        let action = app.handle_key(key).await;
                        if let Some(new_events) = action.new_events {
                            events = new_events;
                        }
                        if action.quit {
                            break;
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => app.handle_mouse_event(mouse),
                    Some(Ok(Event::Paste(text))) => app.paste(&text),
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        app.push_error(format!("terminal input: {error}"));
                    }
                    None => break,
                }
            }
        }
        if app.should_quit() {
            break;
        }
    }

    app.cancel_running();
    drop(app);
    for _ in 0..20 {
        if Arc::strong_count(&runtime) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if let Ok(runtime) = Arc::try_unwrap(runtime) {
        let _ = runtime.into_inner().shutdown().await;
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>, TuiError> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|error| TuiError::Terminal(error.to_string()))?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<(), TuiError> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}
