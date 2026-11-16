// Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): updated CLI tests for removed keybinds/press (R-02) and added hl --json contract tests (R-04).
mod support;

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};

use hl_v2::{
    command::Command,
    core::{CommandExecutor, ExecuteFuture},
    execution::{ExecutionStatus, SubmitReceipt},
    ipc::{self, IpcApp},
    metrics::Metrics,
    runtime::{CommandQueue, CommandService},
    scope::ScopeRegistry,
};
use tokio::sync::RwLock;

use support::{now_ms, ready_perp};

struct NoopExecutor;

impl CommandExecutor for NoopExecutor {
    fn execute_command<'a>(&'a self, _command: Command) -> ExecuteFuture<'a> {
        accepted()
    }

    fn execute_command_for<'a>(&'a self, _symbol: &'a str, _command: Command) -> ExecuteFuture<'a> {
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

fn temp_dir() -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    std::env::temp_dir().join(format!(
        "hl-v2-binary-{}-{}-{}",
        std::process::id(),
        now_ms(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn run_hl(data_dir: &Path, socket: &Path, args: &[&str]) -> std::process::Output {
    ProcessCommand::new(env!("CARGO_BIN_EXE_hl"))
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--ipc-path")
        .arg(socket)
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_client_enforces_arity_health_exit_and_typed_quit() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("runtime").join("backend.sock");
    let state = Arc::new(RwLock::new(ready_perp("0")));
    let metrics = Metrics::default();
    let service = Arc::new(
        CommandService::new(state.clone(), Arc::new(NoopExecutor))
            .with_scopes(ScopeRegistry::new("BTC".to_string()).unwrap()),
    );
    let app = IpcApp::new(state, metrics.clone(), CommandQueue::new(service).unwrap());
    let socket_for_server = socket.clone();
    let server = tokio::spawn(async move {
        ipc::serve(&socket_for_server, app, 1_048_576)
            .await
            .unwrap()
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(socket.exists());

    let healthy = run_hl(&dir, &socket, &["health"]);
    assert!(
        healthy.status.success(),
        "{}",
        String::from_utf8_lossy(&healthy.stderr)
    );
    assert!(String::from_utf8_lossy(&healthy.stdout).contains("\"ok\": true"));

    let scoped_health = run_hl(&dir, &socket, &["--market", "BTC", "health"]);
    assert!(
        scoped_health.status.success(),
        "{}",
        String::from_utf8_lossy(&scoped_health.stderr)
    );
    let metrics_output = run_hl(&dir, &socket, &["MeTrIcS"]);
    assert!(metrics_output.status.success());

    let exec = run_hl(&dir, &socket, &["exec", "status"]);
    assert!(exec.status.success());
    assert!(String::from_utf8_lossy(&exec.stdout).contains("active=BTC"));

    // R-02: keybinds and `hl press` are removed and fail closed.
    let bind = run_hl(&dir, &socket, &["bind", "k", "status"]);
    assert!(!bind.status.success());
    let press = run_hl(&dir, &socket, &["press", "k"]);
    assert!(!press.status.success());

    let clear = run_hl(&dir, &socket, &["clear"]);
    assert!(clear.status.success());
    assert!(clear.stdout.starts_with(b"\x1b[2J\x1b[H"));

    let extra = run_hl(&dir, &socket, &["health", "extra"]);
    assert!(!extra.status.success());
    assert!(String::from_utf8_lossy(&extra.stderr).contains("does not accept arguments"));

    let command_extra = run_hl(&dir, &socket, &["command", "get", "1", "extra"]);
    assert!(!command_extra.status.success());
    let unknown_command = run_hl(&dir, &socket, &["command", "get", "999999"]);
    assert!(!unknown_command.status.success());

    let quit = run_hl(&dir, &socket, &["q"]);
    assert!(
        quit.status.success(),
        "{}",
        String::from_utf8_lossy(&quit.stderr)
    );

    metrics.state_ws_disconnected();
    let unhealthy = run_hl(&dir, &socket, &["health"]);
    assert!(!unhealthy.status.success());
    assert!(String::from_utf8_lossy(&unhealthy.stdout).contains("\"ok\": false"));

    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r04_hl_json_contract_and_e2e_agent_scenario() {
    use hl_v2::{
        ipc::{IpcRequest, IpcResponse},
        protocol::{AssetId, MarketKind, TimeInForce},
        runtime::{CommandSubmitResponse, ReadOnlyExecutor},
        state::{ActiveAssetData, Book, Fill, Market, Order, OrderKind, Position, TradingState},
    };
    use serde_json::Value;
    use support::dec;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn parse_single_json_stdout(output: &std::process::Output) -> Value {
        let raw = &output.stdout;
        assert!(
            raw.ends_with(b"\n"),
            "stdout must end with newline: {:?}",
            String::from_utf8_lossy(raw)
        );
        assert!(
            !raw.contains(&0x1b),
            "stdout must contain zero ANSI escapes: {:?}",
            String::from_utf8_lossy(raw)
        );
        let trimmed = &raw[..raw.len() - 1];
        assert!(
            !trimmed.contains(&b'\n'),
            "stdout must contain exactly one JSON line: {:?}",
            String::from_utf8_lossy(raw)
        );
        serde_json::from_slice(trimmed).expect("valid single-object JSON on stdout")
    }

    fn multi_market_state() -> TradingState {
        let now = now_ms();
        let mut state = ready_perp("0.25");
        state.fill_session_start_ms = now.saturating_sub(1_000);
        state.set_market(Market {
            symbol: "ETH".to_string(),
            wire_symbol: "ETH".to_string(),
            dex: None,
            asset: AssetId::native_perp(1),
            kind: MarketKind::Perp,
            size_decimals: 4,
            max_leverage: Some(25),
            delisted: false,
            open_interest_cap: false,
        });
        state.apply_book(
            "ETH",
            Book {
                bid: dec("3000"),
                ask: dec("3001"),
            },
            now,
            Some(now),
        );
        state.apply_active_asset(
            "ETH",
            ActiveAssetData {
                leverage: 10,
                leverage_cross: true,
                max_trade_sizes: [dec("100"), dec("100")],
                available_to_trade: [dec("100"), dec("100")],
                mark_price: dec("3000.5"),
            },
            now,
            Some(now),
        );
        state.apply_position_snapshot(
            Position {
                symbol: "ETH".to_string(),
                size: dec("1.5"),
                entry_price: Some(dec("2950")),
                detail: None,
            },
            now,
            Some(now),
        );
        state.apply_orders_snapshot(
            "BTC",
            vec![Order {
                fast_cancel_eligible: false,
                trailing: None,
                symbol: "BTC".to_string(),
                oid: 101,
                cloid: None,
                is_buy: true,
                price: dec("48000"),
                size: dec("0.1"),
                reduce_only: false,
                kind: OrderKind::Limit,
                tif: Some(TimeInForce::Gtc),
            }],
            now,
            Some(now),
        );
        state.apply_orders_snapshot(
            "ETH",
            vec![Order {
                fast_cancel_eligible: false,
                trailing: None,
                symbol: "ETH".to_string(),
                oid: 202,
                cloid: None,
                is_buy: false,
                price: dec("3200"),
                size: dec("0.5"),
                reduce_only: true,
                kind: OrderKind::Limit,
                tif: Some(TimeInForce::Gtc),
            }],
            now,
            Some(now),
        );
        state.record_fill(Fill {
            seq: 0,
            tid: 1,
            oid: 101,
            symbol: "BTC".to_string(),
            is_buy: true,
            size: dec("0.1"),
            price: dec("49900"),
            closed_pnl: dec("0"),
            time_ms: now,
        });
        state.record_fill(Fill {
            seq: 0,
            tid: 2,
            oid: 202,
            symbol: "ETH".to_string(),
            is_buy: false,
            size: dec("0.5"),
            price: dec("3010"),
            closed_pnl: dec("12.5"),
            time_ms: now,
        });
        state
    }

    // (1) §5.1 End-to-end CLI agent scenario over `ReadOnlyExecutor` + multi-market (`BTC`, `ETH`) `TradingState`.
    {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("runtime").join("backend.sock");
        let state = Arc::new(RwLock::new(multi_market_state()));
        let metrics = Metrics::default();
        metrics.state_ws_connected();
        let service = Arc::new(
            CommandService::new(state.clone(), Arc::new(ReadOnlyExecutor))
                .with_scopes(ScopeRegistry::new("BTC".to_string()).unwrap()),
        );
        let app = IpcApp::new(state, metrics, CommandQueue::new(service).unwrap());
        let socket_for_server = socket.clone();
        let server = tokio::spawn(async move {
            ipc::serve(&socket_for_server, app, 1_048_576)
                .await
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(socket.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::symlink_metadata(socket.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::symlink_metadata(&socket)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let health = run_hl(&dir, &socket, &["--json", "health"]);
        assert_eq!(health.status.code(), Some(0));
        let health_json = parse_single_json_stdout(&health);
        assert_eq!(health_json["ok"], true);
        assert_eq!(health_json["type"], "health");
        assert_eq!(health_json["payload"]["active"], "BTC");

        // Scoped `--market ETH` reads reflect `ETH` while global `state.active` remains `BTC`.
        let eth_status = run_hl(&dir, &socket, &["--market", "ETH", "--json", "status"]);
        assert_eq!(eth_status.status.code(), Some(0));
        let eth_status_json = parse_single_json_stdout(&eth_status);
        assert_eq!(eth_status_json["ok"], true);
        assert_eq!(eth_status_json["type"], "status");
        assert_eq!(eth_status_json["payload"]["active"], "ETH");
        assert_eq!(eth_status_json["payload"]["openOrders"], 1);
        assert_eq!(eth_status_json["payload"]["position"]["symbol"], "ETH");

        let eth_orders = run_hl(&dir, &socket, &["--market", "ETH", "--json", "orders"]);
        assert_eq!(eth_orders.status.code(), Some(0));
        let eth_orders_json = parse_single_json_stdout(&eth_orders);
        assert_eq!(eth_orders_json["ok"], true);
        assert_eq!(
            eth_orders_json["payload"]["orders"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(eth_orders_json["payload"]["orders"][0]["symbol"], "ETH");

        let eth_pos = run_hl(&dir, &socket, &["--market", "ETH", "--json", "position"]);
        assert_eq!(eth_pos.status.code(), Some(0));
        let eth_pos_json = parse_single_json_stdout(&eth_pos);
        assert_eq!(eth_pos_json["ok"], true);
        assert_eq!(
            eth_pos_json["payload"]["positions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(eth_pos_json["payload"]["positions"][0]["symbol"], "ETH");

        let eth_health = run_hl(&dir, &socket, &["--market", "ETH", "--json", "health"]);
        assert_eq!(eth_health.status.code(), Some(0));
        let eth_health_json = parse_single_json_stdout(&eth_health);
        assert_eq!(eth_health_json["ok"], true);
        assert_eq!(eth_health_json["payload"]["active"], "ETH");

        let eth_fills = run_hl(&dir, &socket, &["--market", "ETH", "--json", "fills"]);
        assert_eq!(eth_fills.status.code(), Some(0));
        let eth_fills_json = parse_single_json_stdout(&eth_fills);
        assert_eq!(eth_fills_json["ok"], true);
        assert_eq!(
            eth_fills_json["payload"]["fills"].as_array().unwrap().len(),
            1
        );
        assert_eq!(eth_fills_json["payload"]["fills"][0]["symbol"], "ETH");

        for (cmd, expected_type) in [
            ("balances", "balances"),
            ("markets", "markets"),
            ("metrics", "metrics"),
        ] {
            let out = run_hl(&dir, &socket, &["--json", cmd]);
            assert_eq!(out.status.code(), Some(0), "failed for {cmd}");
            let parsed = parse_single_json_stdout(&out);
            assert_eq!(parsed["ok"], true);
            assert_eq!(parsed["type"], expected_type);
        }

        // Read-only rejection of mutating command emits valid JSON with `exit 1`, `outcomeUnknown: false`.
        let ro_buy = run_hl(&dir, &socket, &["--market", "BTC", "--json", "buy", "0.01"]);
        assert_eq!(ro_buy.status.code(), Some(1));
        let ro_buy_json = parse_single_json_stdout(&ro_buy);
        assert_eq!(ro_buy_json["ok"], false);
        assert_eq!(ro_buy_json["type"], "command_execution");
        assert_eq!(ro_buy_json["submitted"], true);
        assert_eq!(ro_buy_json["outcomeUnknown"], false);
        assert_eq!(ro_buy_json["retrySafe"], false);
        assert!(
            ro_buy_json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("read-only")
        );

        // Pre-enqueue rejection (`> 64 KiB` command) returns `exit 1`, `submitted: false`, `retrySafe: true`.
        let oversized = format!("buy {}", "1".repeat(66_000));
        let too_big = run_hl(&dir, &socket, &["--json", &oversized]);
        assert_eq!(too_big.status.code(), Some(1));
        let too_big_json = parse_single_json_stdout(&too_big);
        assert_eq!(too_big_json["ok"], false);
        assert_eq!(too_big_json["submitted"], false);
        assert_eq!(too_big_json["outcomeUnknown"], false);
        assert_eq!(too_big_json["retrySafe"], true);
        assert!(too_big_json["commandId"].is_null());

        server.abort();
        let _ = server.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    // (2) Mutating command (`exit 0` + `CommandExecutionRecord` & `SubmitReceipt`), `command get`, and `Ambiguous` receipt (`exit 2`).
    {
        struct ModeExecutor {
            ambiguous: std::sync::atomic::AtomicBool,
        }
        impl CommandExecutor for ModeExecutor {
            fn execute_command<'a>(&'a self, _command: Command) -> ExecuteFuture<'a> {
                let amb = self.ambiguous.load(Ordering::Relaxed);
                Box::pin(async move {
                    Ok(SubmitReceipt {
                        id: 7,
                        status: if amb {
                            ExecutionStatus::Ambiguous
                        } else {
                            ExecutionStatus::Accepted
                        },
                        statuses: Vec::new(),
                        error: amb.then(|| "ws stream closed".to_string()),
                    })
                })
            }
            fn execute_command_for<'a>(
                &'a self,
                _symbol: &'a str,
                command: Command,
            ) -> ExecuteFuture<'a> {
                self.execute_command(command)
            }
        }

        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("runtime").join("backend.sock");
        let state = Arc::new(RwLock::new(ready_perp("0")));
        let exec = Arc::new(ModeExecutor {
            ambiguous: std::sync::atomic::AtomicBool::new(false),
        });
        let service = Arc::new(CommandService::new(state.clone(), exec.clone()));
        let app = IpcApp::new(
            state,
            Metrics::default(),
            CommandQueue::new(service).unwrap(),
        );
        let socket_for_server = socket.clone();
        let server = tokio::spawn(async move {
            ipc::serve(&socket_for_server, app, 1_048_576)
                .await
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let buy = run_hl(&dir, &socket, &["--json", "buy", "0.01"]);
        assert_eq!(buy.status.code(), Some(0));
        let buy_json = parse_single_json_stdout(&buy);
        assert_eq!(buy_json["ok"], true);
        assert_eq!(buy_json["type"], "command_execution");
        assert_eq!(buy_json["payload"]["commandId"], 1);
        assert_eq!(
            buy_json["payload"]["response"]["receipts"][0]["status"],
            "accepted"
        );

        let cmd_get = run_hl(&dir, &socket, &["--json", "command", "get", "1"]);
        assert_eq!(cmd_get.status.code(), Some(0));
        let cmd_get_json = parse_single_json_stdout(&cmd_get);
        assert_eq!(cmd_get_json["ok"], true);
        assert_eq!(cmd_get_json["type"], "command_get");
        assert_eq!(cmd_get_json["payload"]["commandId"], 1);

        exec.ambiguous.store(true, Ordering::Relaxed);
        let amb_buy = run_hl(&dir, &socket, &["--json", "buy", "0.01"]);
        assert_eq!(amb_buy.status.code(), Some(2));
        let amb_json = parse_single_json_stdout(&amb_buy);
        assert_eq!(amb_json["ok"], false);
        assert_eq!(amb_json["type"], "command_execution");
        assert_eq!(amb_json["submitted"], true);
        assert_eq!(amb_json["outcomeUnknown"], true);
        assert_eq!(amb_json["retrySafe"], false);
        assert_eq!(amb_json["commandId"], 2);

        server.abort();
        let _ = server.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    // (3) Pre-submit errors (`hl --json` with no args, missing socket) exit `1` with `submitted: false, retrySafe: true`.
    {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let missing_socket = dir.join("runtime").join("missing.sock");

        let no_cmd = run_hl(&dir, &missing_socket, &["--json"]);
        assert_eq!(no_cmd.status.code(), Some(1));
        let no_cmd_json = parse_single_json_stdout(&no_cmd);
        assert_eq!(no_cmd_json["ok"], false);
        assert_eq!(no_cmd_json["submitted"], false);
        assert_eq!(no_cmd_json["outcomeUnknown"], false);
        assert_eq!(no_cmd_json["retrySafe"], true);
        assert!(no_cmd_json["commandId"].is_null());

        let missing = run_hl(&dir, &missing_socket, &["--json", "buy", "0.01"]);
        assert_eq!(missing.status.code(), Some(1));
        let missing_json = parse_single_json_stdout(&missing);
        assert_eq!(missing_json["ok"], false);
        assert_eq!(missing_json["submitted"], false);
        assert_eq!(missing_json["outcomeUnknown"], false);
        assert_eq!(missing_json["retrySafe"], true);
        assert!(missing_json["commandId"].is_null());

        // R-04 regression: client argument-parse failures under `--json` (invalid `--wait-ms`, unknown
        // flag, invalid `HL_V2_WAIT_MS`) emit the pre-enqueue error envelope and exit `1`, never an
        // empty stdout with the ambiguous exit code `2`.
        let invalid_wait = run_hl(
            &dir,
            &missing_socket,
            &["--json", "--wait-ms", "abc", "status"],
        );
        let unknown_flag = run_hl(&dir, &missing_socket, &["--json", "--bogus-flag", "status"]);
        let invalid_wait_env = ProcessCommand::new(env!("CARGO_BIN_EXE_hl"))
            .env("HL_V2_WAIT_MS", "abc")
            .arg("--data-dir")
            .arg(&dir)
            .arg("--ipc-path")
            .arg(&missing_socket)
            .args(["--json", "status"])
            .output()
            .unwrap();
        for (case, output) in [
            ("invalid --wait-ms", &invalid_wait),
            ("unknown flag", &unknown_flag),
            ("invalid HL_V2_WAIT_MS", &invalid_wait_env),
        ] {
            assert_eq!(output.status.code(), Some(1), "{case}");
            let parsed = parse_single_json_stdout(output);
            assert_eq!(parsed["ok"], false, "{case}");
            assert_eq!(parsed["type"], "error", "{case}");
            assert_eq!(parsed["submitted"], false, "{case}");
            assert_eq!(parsed["outcomeUnknown"], false, "{case}");
            assert_eq!(parsed["retrySafe"], true, "{case}");
            assert!(parsed["commandId"].is_null(), "{case}");
            assert!(
                parsed["error"].as_str().is_some_and(|e| !e.is_empty()),
                "{case}"
            );
        }

        // Without `--json`, argument-parse failures keep the standard clap usage error behaviour.
        let plain_invalid_wait = run_hl(&dir, &missing_socket, &["--wait-ms", "abc", "status"]);
        assert_eq!(plain_invalid_wait.status.code(), Some(2));
        assert!(plain_invalid_wait.stdout.is_empty());
        assert!(!plain_invalid_wait.stderr.is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }

    // (4) Sensitivity Hint #1: Drop listener after `CommandSubmit` (`command_id = 1`) and before `CommandGet` -> `exit 2`!
    {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("runtime").join("backend.sock");
        let listener = ipc::bind(&socket).await.unwrap();
        let drop_after_submit = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let len = stream.read_u32().await.unwrap() as usize;
            let mut req_bytes = vec![0_u8; len];
            stream.read_exact(&mut req_bytes).await.unwrap();
            let req: IpcRequest = rmp_serde::from_slice(&req_bytes).unwrap();
            assert!(matches!(req, IpcRequest::CommandSubmit { .. }));
            let resp = IpcResponse::CommandSubmit(CommandSubmitResponse {
                command_id: 1,
                accepted_at_ms: now_ms(),
                queued_ahead: 0,
                queue_depth: 1,
                running_command_id: Some(1),
            });
            let payload = rmp_serde::to_vec_named(&resp).unwrap();
            stream.write_u32(payload.len() as u32).await.unwrap();
            stream.write_all(&payload).await.unwrap();
            stream.flush().await.unwrap();
            // Drop both stream and listener so subsequent `CommandGet` fails immediately!
        });

        let out = run_hl(&dir, &socket, &["--json", "buy", "0.01"]);
        drop_after_submit.await.unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "must exit 2 (not 1) when CommandGet fails after CommandSubmit"
        );
        let parsed = parse_single_json_stdout(&out);
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["type"], "command_outcome_unknown");
        assert_eq!(parsed["commandId"], 1);
        assert_eq!(parsed["submitted"], true);
        assert_eq!(parsed["outcomeUnknown"], true);
        assert_eq!(parsed["retrySafe"], false);
        assert!(
            parsed["error"]
                .as_str()
                .unwrap_or_default()
                .contains("do not resubmit; check with: hl --json command get 1")
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    // (5) `--wait-ms` polling timeout after `CommandSubmit` also exits `2` (`command_outcome_unknown`).
    {
        struct SlowExecutor;
        impl CommandExecutor for SlowExecutor {
            fn execute_command<'a>(&'a self, _command: Command) -> ExecuteFuture<'a> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    accepted().await
                })
            }
            fn execute_command_for<'a>(
                &'a self,
                _symbol: &'a str,
                command: Command,
            ) -> ExecuteFuture<'a> {
                self.execute_command(command)
            }
        }

        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("runtime").join("backend.sock");
        let state = Arc::new(RwLock::new(ready_perp("0")));
        let service = Arc::new(CommandService::new(state.clone(), Arc::new(SlowExecutor)));
        let app = IpcApp::new(
            state,
            Metrics::default(),
            CommandQueue::new(service).unwrap(),
        );
        let socket_for_server = socket.clone();
        let server = tokio::spawn(async move {
            ipc::serve(&socket_for_server, app, 1_048_576)
                .await
                .unwrap()
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let timed_out = run_hl(&dir, &socket, &["--wait-ms", "40", "--json", "buy", "0.01"]);
        assert_eq!(timed_out.status.code(), Some(2));
        let parsed = parse_single_json_stdout(&timed_out);
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["type"], "command_outcome_unknown");
        assert_eq!(parsed["commandId"], 1);
        assert_eq!(parsed["submitted"], true);
        assert_eq!(parsed["outcomeUnknown"], true);
        assert_eq!(parsed["retrySafe"], false);

        server.abort();
        let _ = server.await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_shell_restores_the_exact_prior_terminal_state() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("runtime").join("backend.sock");
    let state = Arc::new(RwLock::new(ready_perp("0")));
    let service = Arc::new(CommandService::new(state.clone(), Arc::new(NoopExecutor)));
    let app = IpcApp::new(
        state,
        Metrics::default(),
        CommandQueue::new(service).unwrap(),
    );
    let socket_for_server = socket.clone();
    let server = tokio::spawn(async move {
        ipc::serve(&socket_for_server, app, 1_048_576)
            .await
            .unwrap()
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let mut master = 0;
    let mut slave = 0;
    // SAFETY: openpty initializes both descriptors on success; they are immediately owned by Files.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned unique live descriptors above.
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    // SAFETY: openpty returned unique live descriptors above.
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let mut expected = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr initializes expected on success for the live PTY slave descriptor.
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), expected.as_mut_ptr()) },
        0
    );
    // SAFETY: tcgetattr succeeded and initialized the value.
    let mut expected = unsafe { expected.assume_init() };
    expected.c_lflag &= !libc::ECHO;
    expected.c_cc[libc::VMIN] = 2;
    expected.c_cc[libc::VTIME] = 1;
    // SAFETY: expected is a valid termios value for the live PTY slave descriptor.
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &expected) },
        0
    );

    let duplicate = |fd| {
        // SAFETY: fd is the live PTY slave descriptor and dup returns independent ownership.
        let duplicated = unsafe { libc::dup(fd) };
        assert!(duplicated >= 0);
        // SAFETY: duplicated is a new descriptor owned by the returned File.
        unsafe { std::fs::File::from_raw_fd(duplicated) }
    };
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_hl"))
        .arg("--data-dir")
        .arg(&dir)
        .arg("--ipc-path")
        .arg(&socket)
        .stdin(Stdio::from(duplicate(slave.as_raw_fd())))
        .stdout(Stdio::from(duplicate(slave.as_raw_fd())))
        .stderr(Stdio::from(duplicate(slave.as_raw_fd())))
        .spawn()
        .unwrap();
    master.write_all(b"q\n").unwrap();

    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(status.success());

    let mut actual = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr initializes actual on success for the still-live PTY slave descriptor.
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), actual.as_mut_ptr()) },
        0
    );
    // SAFETY: tcgetattr succeeded and initialized the value.
    let actual = unsafe { actual.assume_init() };
    assert_eq!(actual.c_iflag, expected.c_iflag);
    assert_eq!(actual.c_oflag, expected.c_oflag);
    assert_eq!(actual.c_cflag, expected.c_cflag);
    // macOS sets PENDIN as transient kernel state when queued PTY input crosses a
    // canonical-mode transition; it is not a user-configured terminal setting.
    assert_eq!(
        actual.c_lflag & !libc::PENDIN,
        expected.c_lflag & !libc::PENDIN
    );
    assert_eq!(actual.c_cc, expected.c_cc);

    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(dir).unwrap();
}
