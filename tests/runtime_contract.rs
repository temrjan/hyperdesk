// Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): updated runtime tests for removed keybinds/@variables/chains (R-02) and added queue bound tests (R-09).
mod support;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use hl_v2::{
    command::Command,
    config::Config,
    core::{CommandExecutor, ExecuteFuture},
    execution::{ExecutionControl, ExecutionStatus, SubmitReceipt},
    metrics::Metrics,
    runtime::{CommandExecutionState, CommandQueue, CommandService, RefreshFuture, StateRefresher},
    scope::ScopeRegistry,
    state::{AccountMode, BalanceSummary},
};
use tokio::sync::RwLock;

use support::{dec, now_ms, ready_perp};

#[derive(Default)]
struct RecordingExecutor {
    commands: Mutex<Vec<(Option<String>, Command)>>,
}

struct SuccessfulRefresher;

impl StateRefresher for SuccessfulRefresher {
    fn refresh_market<'a>(&'a self, _symbol: &'a str) -> RefreshFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

impl CommandExecutor for RecordingExecutor {
    fn execute_command<'a>(&'a self, command: Command) -> ExecuteFuture<'a> {
        self.commands.lock().unwrap().push((None, command));
        accepted()
    }

    fn execute_command_for<'a>(&'a self, symbol: &'a str, command: Command) -> ExecuteFuture<'a> {
        self.commands
            .lock()
            .unwrap()
            .push((Some(symbol.to_string()), command));
        accepted()
    }
}

fn accepted<'a>() -> ExecuteFuture<'a> {
    Box::pin(async {
        Ok(SubmitReceipt {
            id: 1,
            status: ExecutionStatus::Accepted,
            statuses: Vec::new(),
            error: None,
        })
    })
}

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hl-v2-{label}-{}-{}", std::process::id(), now_ms()))
}

fn service(executor: Arc<RecordingExecutor>) -> Arc<CommandService> {
    Arc::new(CommandService::new(
        Arc::new(RwLock::new(ready_perp("0"))),
        executor,
    ))
}

#[tokio::test]
async fn account_wide_actions_require_typed_confirmation() {
    let executor = Arc::new(RecordingExecutor::default());
    let service = service(executor.clone());

    let prompt = service.execute_line("account mode set unified").await;
    assert!(prompt.ok);
    assert!(prompt.lines[0].contains("reply yes or no"));
    assert!(executor.commands.lock().unwrap().is_empty());

    let blocked = service.execute_line("buy 0.01").await;
    assert!(!blocked.ok);
    assert!(blocked.lines[0].contains("confirmation pending"));

    assert!(service.execute_line("no").await.ok);
    assert!(executor.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn impossible_account_mode_transition_never_stages_confirmation() {
    let executor = Arc::new(RecordingExecutor::default());
    let mut state = ready_perp("0");
    let now = now_ms();
    state.apply_account_mode(AccountMode::UnifiedAccount, now, Some(now));
    let state = Arc::new(RwLock::new(state));
    let service = CommandService::new(state.clone(), executor.clone());

    let response = service.execute_line("account mode set standard").await;
    assert!(!response.ok);
    assert!(response.lines[0].contains("requires the main user signer"));
    assert!(executor.commands.lock().unwrap().is_empty());

    state
        .write()
        .await
        .apply_account_mode(AccountMode::Standard, 0, None);
    let response = service.execute_line("account mode set unified").await;
    assert!(!response.ok);
    assert!(response.lines[0].contains("account mode stale"));
    assert!(executor.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn account_wide_confirmation_is_unique_across_market_sessions() {
    let executor = Arc::new(RecordingExecutor::default());
    let scopes = ScopeRegistry::new("BTC".to_string()).unwrap();
    let service = CommandService::new(Arc::new(RwLock::new(ready_perp("0"))), executor)
        .with_scopes(scopes.clone());
    let first = scopes.open("BTC".to_string()).await;
    let second = scopes.open("BTC".to_string()).await;

    assert!(
        service
            .execute_line_scoped(first, "account mode set unified")
            .await
            .ok
    );
    let blocked = service
        .execute_line_scoped(second, "account mode set portfolioMargin")
        .await;
    assert!(!blocked.ok);
    assert!(blocked.lines[0].contains("account-wide confirmation already pending"));
    assert!(blocked.lines[0].contains("its owner must reply"));
    service.close_market_session(first).await.unwrap();
    let replacement = service
        .execute_line_scoped(second, "account mode set portfolioMargin")
        .await;
    assert!(replacement.ok, "{replacement:?}");
    assert!(service.execute_line_scoped(second, "no").await.ok);
}

#[tokio::test]
async fn quit_clear_and_chain_control_are_typed_not_text_conventions() {
    // R-02: typed quit and clear remain active while `;` and `wait`/`sleep` chains fail closed.
    let service = service(Arc::new(RecordingExecutor::default()));
    let quit = service.execute_line("q").await;
    assert!(quit.ok);
    assert!(quit.exit);

    let clear = service.execute_line("clear").await;
    assert!(clear.ok);
    assert!(clear.clear);
    assert!(clear.lines.is_empty());

    let chain = service.execute_line("status; q").await;
    assert!(!chain.ok);
    let wait = service.execute_line("wait 1ms").await;
    assert!(!wait.ok);
}

#[tokio::test]
async fn every_read_policy_and_shell_family_executes_through_runtime() {
    let service = CommandService::new(
        Arc::new(RwLock::new(ready_perp("0"))),
        Arc::new(RecordingExecutor::default()),
    )
    .with_config(&Config::defaults(temp_path("surface-config")))
    .with_refresher(Arc::new(SuccessfulRefresher))
    .with_diagnostics(Metrics::default(), ExecutionControl::default(), None);
    for command in [
        "help",
        "help all",
        "status",
        "portfolio",
        "balances",
        "orders",
        "position",
        "risk",
        "config",
        "config get market-cross-bps",
        "doctor",
        "refresh",
        "markets",
        "markets perps",
        "markets hip3",
        "markets spot",
        "instrument",
        "instrument BTC",
        "account mode",
        "account mode require any",
    ] {
        let response = service.execute_line(command).await;
        assert!(response.ok, "{command:?}: {response:?}");
    }
}

struct DelayedExecutor {
    delay: Duration,
}

impl CommandExecutor for DelayedExecutor {
    fn execute_command<'a>(&'a self, _command: Command) -> ExecuteFuture<'a> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            accepted().await
        })
    }

    fn execute_command_for<'a>(&'a self, _symbol: &'a str, _command: Command) -> ExecuteFuture<'a> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            accepted().await
        })
    }
}

#[tokio::test]
async fn retained_scoped_job_finishes_after_session_close() {
    let executor = Arc::new(DelayedExecutor {
        delay: Duration::from_millis(50),
    });
    let scopes = ScopeRegistry::new("BTC".to_string()).unwrap();
    let service = Arc::new(
        CommandService::new(Arc::new(RwLock::new(ready_perp("0"))), executor)
            .with_scopes(scopes.clone()),
    );
    let queue = CommandQueue::new(service).unwrap();
    let session = queue.open_market_session("BTC".to_string()).await.unwrap();
    let submitted = queue
        .submit_scoped(session.session_id, "buy 0.01".to_string())
        .await
        .unwrap();
    queue
        .close_market_session(session.session_id)
        .await
        .unwrap();
    assert!(queue.market_session(session.session_id).await.is_err());

    for _ in 0..100 {
        if let Some(record) = queue.get(submitted.command_id).await
            && record.state == CommandExecutionState::Completed
        {
            let response = record.response.unwrap();
            assert!(response.ok, "{response:?}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("retained scoped job did not complete");
}

#[tokio::test]
async fn account_summary_and_doctor_report_the_authoritative_source_and_causes() {
    let mut seeded = ready_perp("0");
    let now = now_ms();
    seeded.apply_account_mode(AccountMode::UnifiedAccount, now, Some(now));
    seeded.apply_spot_balances(
        Vec::new(),
        BalanceSummary {
            portfolio_margin_enabled: false,
            portfolio_margin_ratio: None,
            spot_value_usd: Some(dec("1234")),
            spot_available_usd: Some(dec("1200")),
            spot_unpriced_count: 0,
            borrow_lend_health: None,
            borrow_lend_health_factor: None,
        },
        now,
        Some(now),
    );
    let state = Arc::new(RwLock::new(seeded));
    assert_eq!(
        state.read().await.account_overview().unwrap().source,
        "spotClearinghouseState"
    );

    let metrics = Metrics::default();
    metrics.state_ws_disconnected();
    let execution = ExecutionControl::default();
    execution.halt("ambiguous action".to_string()).await;
    let service = CommandService::new(state, Arc::new(RecordingExecutor::default()))
        .with_diagnostics(metrics, execution, None);
    let doctor = service.execute_line("doctor").await;
    assert!(!doctor.ok);
    assert!(
        doctor
            .lines
            .iter()
            .any(|line| line.contains("state feed disconnected"))
    );
    assert!(
        doctor
            .lines
            .iter()
            .any(|line| line.contains("ambiguous action"))
    );
}

struct GatedExecutor {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl CommandExecutor for GatedExecutor {
    fn execute_command<'a>(&'a self, _command: Command) -> ExecuteFuture<'a> {
        Box::pin(async move {
            self.started.notify_waiters();
            self.release.notified().await;
            accepted().await
        })
    }

    fn execute_command_for<'a>(&'a self, _symbol: &'a str, _command: Command) -> ExecuteFuture<'a> {
        Box::pin(async move {
            self.started.notify_waiters();
            self.release.notified().await;
            accepted().await
        })
    }
}

#[tokio::test]
async fn r09_command_queue_enforces_size_and_depth_bounds_without_leaking_scope_jobs() {
    use hl_v2::{
        ipc::{IpcApp, IpcRequest, IpcResponse},
        runtime::{MAX_COMMAND_BYTES, MAX_TRACKED_SUBMISSIONS},
    };

    let gate = Arc::new(GatedExecutor {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scopes = ScopeRegistry::new("BTC".to_string()).unwrap();
    let state = Arc::new(RwLock::new(ready_perp("0")));
    let service = Arc::new(CommandService::new(state.clone(), gate.clone()).with_scopes(scopes));
    let queue = CommandQueue::new(service).unwrap();
    let app = IpcApp::new(state, Metrics::default(), queue.clone());

    let session = match app
        .dispatch(IpcRequest::MarketSessionOpen {
            market: "BTC".to_string(),
        })
        .await
    {
        IpcResponse::MarketSession(session) => session,
        other => panic!("expected MarketSession, got {other:?}"),
    };

    // Oversized commands (> 64 KiB) fail on both global and scoped submit without leaking jobs.
    let oversized = format!("buy {}", "1".repeat(MAX_COMMAND_BYTES));
    assert!(matches!(
        app.dispatch(IpcRequest::CommandSubmit {
            command: oversized.clone(),
        })
        .await,
        IpcResponse::Error { .. }
    ));
    assert!(matches!(
        app.dispatch(IpcRequest::CommandSubmitScoped {
            session_id: session.session_id,
            command: oversized,
        })
        .await,
        IpcResponse::Error { .. }
    ));

    // Fill queue to exact MAX_TRACKED_SUBMISSIONS (1 running + 255 queued).
    let first_waiter = gate.started.notified();
    let first_id = match app
        .dispatch(IpcRequest::CommandSubmitScoped {
            session_id: session.session_id,
            command: "buy 0.01".to_string(),
        })
        .await
    {
        IpcResponse::CommandSubmit(resp) => resp.command_id,
        other => panic!("expected first CommandSubmit, got {other:?}"),
    };
    first_waiter.await;

    let mut last_id = first_id;
    for _ in 1..MAX_TRACKED_SUBMISSIONS {
        last_id = match app
            .dispatch(IpcRequest::CommandSubmitScoped {
                session_id: session.session_id,
                command: "status".to_string(),
            })
            .await
        {
            IpcResponse::CommandSubmit(resp) => resp.command_id,
            other => panic!("expected queued CommandSubmit, got {other:?}"),
        };
    }

    // 257th submission is rejected on both scoped and unscoped paths and releases scope retain count.
    assert!(matches!(
        app.dispatch(IpcRequest::CommandSubmitScoped {
            session_id: session.session_id,
            command: "status".to_string(),
        })
        .await,
        IpcResponse::Error { .. }
    ));
    assert!(matches!(
        app.dispatch(IpcRequest::CommandSubmit {
            command: "status".to_string(),
        })
        .await,
        IpcResponse::Error { .. }
    ));

    // Release running job and let remaining queued commands complete, then close scoped session cleanly.
    gate.release.notify_one();
    for _ in 0..200 {
        if let Some(record) = queue.get(last_id).await
            && record.state == CommandExecutionState::Completed
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        queue.get(last_id).await.unwrap().state,
        CommandExecutionState::Completed
    );
    assert_eq!(
        app.dispatch(IpcRequest::MarketSessionClose {
            session_id: session.session_id,
        })
        .await,
        IpcResponse::MarketSessionClosed
    );
}
