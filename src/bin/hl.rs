// Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): removed keybind/press commands, migrated RawMode to rustix::termios, enforced #![forbid(unsafe_code)], and added --json mode.
#![forbid(unsafe_code)]

use std::{
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Parser;
use hl_v2::{
    config::Config,
    execution::ExecutionStatus,
    ipc::{self, IpcRequest, IpcResponse, SubmitTransportError},
    runtime::{CommandExecutionState, CommandResponse},
    state::Fill,
};
use rustix::termios::{
    LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcsetattr,
};
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(name = "hl", about = "Hyperliquid v2 thin client")]
struct Args {
    #[arg(long, env = "HL_V2_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "HL_V2_IPC_PATH")]
    ipc_path: Option<PathBuf>,
    #[arg(long, value_name = "SYMBOL")]
    market: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value_t = 30_000, env = "HL_V2_WAIT_MS")]
    wait_ms: u64,
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.json {
        let code = run_json(args).await;
        std::process::exit(code);
    }
    let cfg = Config::load(args.data_dir)?;
    let ipc_path = args
        .ipc_path
        .unwrap_or_else(|| cfg.runtime_dir().join("backend.sock"));
    let session_id = match args.market {
        Some(market) => Some(open_market_session(&ipc_path, market).await?.0),
        None => None,
    };
    let result = if args.command.is_empty() {
        shell(&ipc_path, session_id).await
    } else {
        run_once(&ipc_path, args.command, session_id).await
    };
    if let Some(session_id) = session_id {
        let close = close_market_session(&ipc_path, session_id).await;
        if result.is_ok() {
            close?;
        }
    }
    result
}

async fn open_market_session(path: &Path, market: String) -> anyhow::Result<(u64, String)> {
    match ipc::request(path, &IpcRequest::MarketSessionOpen { market }, max_frame()).await? {
        IpcResponse::MarketSession(session) => Ok((session.session_id, session.active)),
        IpcResponse::Error { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected market session response: {other:?}"),
    }
}

async fn close_market_session(path: &Path, session_id: u64) -> anyhow::Result<()> {
    match ipc::request(
        path,
        &IpcRequest::MarketSessionClose { session_id },
        max_frame(),
    )
    .await?
    {
        IpcResponse::MarketSessionClosed => Ok(()),
        IpcResponse::Error { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected market session close response: {other:?}"),
    }
}

fn emit_json(value: &Value) {
    let text = serde_json::to_string(value).unwrap_or_else(|err| {
        format!(
            r#"{{"ok":false,"type":"error","submitted":false,"outcomeUnknown":false,"retrySafe":true,"commandId":null,"error":"serialize json: {}","payload":null}}"#,
            err
        )
    });
    println!("{text}");
}

fn emit_pre_submit_error(message: &str) -> i32 {
    emit_json(&json!({
        "ok": false,
        "type": "error",
        "submitted": false,
        "outcomeUnknown": false,
        "retrySafe": true,
        "commandId": Value::Null,
        "error": message,
        "payload": Value::Null,
    }));
    1
}

fn emit_outcome_unknown_after_submit(command_id: u64, cause: &str) -> i32 {
    emit_json(&json!({
        "ok": false,
        "type": "command_outcome_unknown",
        "submitted": true,
        "outcomeUnknown": true,
        "retrySafe": false,
        "commandId": command_id,
        "error": format!(
            "command {command_id} accepted by hld; outcome unknown ({cause}) — do not resubmit; check with: hl --json command get {command_id}"
        ),
        "payload": Value::Null,
    }));
    2
}

async fn run_json(args: Args) -> i32 {
    if args.command.is_empty() {
        return emit_pre_submit_error("hl --json requires a command");
    }
    let cfg = match Config::load(args.data_dir) {
        Ok(cfg) => cfg,
        Err(err) => return emit_pre_submit_error(&err.to_string()),
    };
    let ipc_path = args
        .ipc_path
        .unwrap_or_else(|| cfg.runtime_dir().join("backend.sock"));
    let session = match args.market {
        Some(market) => match open_market_session(&ipc_path, market).await {
            Ok(session) => Some(session),
            Err(err) => return emit_pre_submit_error(&err.to_string()),
        },
        None => None,
    };
    let session_id = session.as_ref().map(|(id, _)| *id);
    let session_market = session.as_ref().map(|(_, symbol)| symbol.as_str());
    let code = run_json_command(
        &ipc_path,
        &args.command,
        args.wait_ms,
        session_id,
        session_market,
    )
    .await;
    if let Some(session_id) = session_id {
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            close_market_session(&ipc_path, session_id),
        )
        .await;
    }
    code
}

async fn run_json_command(
    path: &Path,
    words: &[String],
    wait_ms: u64,
    session_id: Option<u64>,
    session_market: Option<&str>,
) -> i32 {
    let first = words
        .first()
        .map(|word| word.to_ascii_lowercase())
        .unwrap_or_default();
    match first.as_str() {
        "health" => {
            if words.len() != 1 {
                return emit_pre_submit_error("health does not accept arguments");
            }
            let request = session_id.map_or(IpcRequest::Health, |session_id| {
                IpcRequest::MarketSessionHealth { session_id }
            });
            match ipc::request(path, &request, max_frame()).await {
                Ok(IpcResponse::Health(payload)) => {
                    if payload.ok {
                        emit_json(&json!({
                            "ok": true,
                            "type": "health",
                            "payload": payload,
                        }));
                        0
                    } else {
                        let error = if payload.reasons.is_empty() {
                            payload
                                .halt_reason
                                .clone()
                                .unwrap_or_else(|| "daemon unhealthy".to_string())
                        } else {
                            payload.reasons.join("; ")
                        };
                        emit_json(&json!({
                            "ok": false,
                            "type": "health",
                            "submitted": false,
                            "outcomeUnknown": false,
                            "retrySafe": true,
                            "commandId": Value::Null,
                            "error": error,
                            "payload": payload,
                        }));
                        1
                    }
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected health response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "status" | "st" => {
            if words.len() != 1 {
                return emit_pre_submit_error("status does not accept arguments");
            }
            let request = session_id.map_or(IpcRequest::Status, |session_id| {
                IpcRequest::MarketSessionStatus { session_id }
            });
            match ipc::request(path, &request, max_frame()).await {
                Ok(IpcResponse::Status(payload)) => {
                    emit_json(&json!({
                        "ok": true,
                        "type": "status",
                        "payload": payload,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected status response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "orders" => {
            if words.len() != 1 {
                return emit_pre_submit_error("orders does not accept arguments");
            }
            match ipc::request(path, &IpcRequest::Orders, max_frame()).await {
                Ok(IpcResponse::Orders(mut payload)) => {
                    if let Some(symbol) = session_market {
                        payload.orders.retain(|order| order.symbol == symbol);
                    }
                    emit_json(&json!({
                        "ok": true,
                        "type": "orders",
                        "payload": payload,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected orders response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "position" | "positions" | "pos" => {
            if words.len() != 1 {
                return emit_pre_submit_error("position does not accept arguments");
            }
            match ipc::request(path, &IpcRequest::Position, max_frame()).await {
                Ok(IpcResponse::Position(mut payload)) => {
                    if let Some(symbol) = session_market {
                        payload
                            .positions
                            .retain(|position| position.symbol == symbol);
                    }
                    emit_json(&json!({
                        "ok": true,
                        "type": "position",
                        "payload": payload,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected position response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "balances" | "portfolio" => {
            if words.len() != 1 {
                return emit_pre_submit_error("balances does not accept arguments");
            }
            match ipc::request(path, &IpcRequest::Balances, max_frame()).await {
                Ok(IpcResponse::Balances(payload)) => {
                    emit_json(&json!({
                        "ok": true,
                        "type": "balances",
                        "payload": payload,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected balances response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "markets" if words.len() == 1 => {
            match ipc::request(path, &IpcRequest::Markets, max_frame()).await {
                Ok(IpcResponse::Markets(payload)) => {
                    emit_json(&json!({
                        "ok": true,
                        "type": "markets",
                        "payload": payload,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected markets response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "fills" => {
            if words.len() != 1 {
                return emit_pre_submit_error("fills does not accept arguments");
            }
            let request = session_id.map_or(IpcRequest::Fills { after_seq: 0 }, |session_id| {
                IpcRequest::FillsScoped {
                    session_id,
                    after_seq: 0,
                }
            });
            match ipc::request(path, &request, max_frame()).await {
                Ok(IpcResponse::Fills(payload)) => {
                    emit_json(&json!({
                        "ok": true,
                        "type": "fills",
                        "payload": payload,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected fills response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "metrics" => {
            if words.len() != 1 {
                return emit_pre_submit_error("metrics does not accept arguments");
            }
            match ipc::request(path, &IpcRequest::Metrics, max_frame()).await {
                Ok(IpcResponse::Metrics(payload)) => {
                    emit_json(&json!({
                        "ok": true,
                        "type": "metrics",
                        "payload": payload,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected metrics response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "command" => {
            if words.len() != 3 || !words[1].eq_ignore_ascii_case("get") {
                return emit_pre_submit_error("command supports exactly: command get <id>");
            }
            let Ok(command_id) = words[2].parse::<u64>() else {
                return emit_pre_submit_error("command id must be an unsigned integer");
            };
            match ipc::request(path, &IpcRequest::CommandGet { command_id }, max_frame()).await {
                Ok(IpcResponse::CommandGet(record)) => {
                    emit_json(&json!({
                        "ok": true,
                        "type": "command_get",
                        "payload": record,
                    }));
                    0
                }
                Ok(IpcResponse::Error { message }) => emit_pre_submit_error(&message),
                Ok(other) => {
                    emit_pre_submit_error(&format!("unexpected command get response: {other:?}"))
                }
                Err(err) => emit_pre_submit_error(&err.to_string()),
            }
        }
        "exec" => {
            if words.len() <= 1 {
                return emit_pre_submit_error("exec requires a command");
            }
            submit_json_command(path, &words[1..].join(" "), wait_ms, session_id).await
        }
        _ => submit_json_command(path, &words.join(" "), wait_ms, session_id).await,
    }
}

async fn submit_json_command(
    path: &Path,
    command: &str,
    wait_ms: u64,
    session_id: Option<u64>,
) -> i32 {
    let request = match session_id {
        Some(session_id) => IpcRequest::CommandSubmitScoped {
            session_id,
            command: command.to_string(),
        },
        None => IpcRequest::CommandSubmit {
            command: command.to_string(),
        },
    };
    let command_id = match ipc::request_submit(path, &request, max_frame()).await {
        Ok(IpcResponse::CommandSubmit(payload)) => payload.command_id,
        Ok(IpcResponse::Error { message }) => return emit_pre_submit_error(&message),
        Ok(other) => {
            return emit_pre_submit_error(&format!(
                "unexpected command submit response: {other:?}"
            ));
        }
        Err(SubmitTransportError::BeforeSend(err)) => {
            return emit_pre_submit_error(&err.to_string());
        }
        Err(SubmitTransportError::AfterSend(err)) => {
            emit_json(&json!({
                "ok": false,
                "type": "command_outcome_unknown",
                "submitted": Value::Null,
                "outcomeUnknown": true,
                "retrySafe": false,
                "commandId": Value::Null,
                "error": format!(
                    "command submission written to hld socket, but response failed ({err}) — do not resubmit; inspect hl --json orders and hl --json position"
                ),
                "payload": Value::Null,
            }));
            return 2;
        }
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
    loop {
        match ipc::request(path, &IpcRequest::CommandGet { command_id }, max_frame()).await {
            Ok(IpcResponse::CommandGet(record)) => {
                if record.state == CommandExecutionState::Completed {
                    let Some(output) = record.response.clone() else {
                        return emit_outcome_unknown_after_submit(
                            command_id,
                            "completed command missing response",
                        );
                    };
                    let has_ambiguous = output
                        .receipts
                        .iter()
                        .any(|receipt| receipt.status == ExecutionStatus::Ambiguous);
                    if has_ambiguous {
                        let summary = output.lines.join("; ");
                        let error = if summary.is_empty() {
                            format!(
                                "command {command_id} completed with ambiguous exchange outcome — do not resubmit; check with: hl --json command get {command_id}"
                            )
                        } else {
                            format!(
                                "{summary} — do not resubmit; check with: hl --json command get {command_id}"
                            )
                        };
                        emit_json(&json!({
                            "ok": false,
                            "type": "command_execution",
                            "submitted": true,
                            "outcomeUnknown": true,
                            "retrySafe": false,
                            "commandId": command_id,
                            "error": error,
                            "payload": record,
                        }));
                        return 2;
                    }
                    if !output.ok {
                        let error = if output.lines.is_empty() {
                            "command failed".to_string()
                        } else {
                            output.lines.join("; ")
                        };
                        emit_json(&json!({
                            "ok": false,
                            "type": "command_execution",
                            "submitted": true,
                            "outcomeUnknown": false,
                            "retrySafe": false,
                            "commandId": command_id,
                            "error": error,
                            "payload": record,
                        }));
                        return 1;
                    }
                    emit_json(&json!({
                        "ok": true,
                        "type": "command_execution",
                        "payload": record,
                    }));
                    return 0;
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return emit_outcome_unknown_after_submit(
                        command_id,
                        &format!("wait timeout after {wait_ms}ms"),
                    );
                }
                tokio::time::sleep((deadline - now).min(Duration::from_millis(50))).await;
            }
            Ok(IpcResponse::Error { message }) => {
                return emit_outcome_unknown_after_submit(command_id, &message);
            }
            Ok(other) => {
                return emit_outcome_unknown_after_submit(
                    command_id,
                    &format!("unexpected command get response: {other:?}"),
                );
            }
            Err(err) => {
                return emit_outcome_unknown_after_submit(command_id, &err.to_string());
            }
        }
    }
}

async fn shell(path: &Path, session_id: Option<u64>) -> anyhow::Result<()> {
    let mut input = ShellInput::new(path, session_id)?;
    loop {
        let prompt = shell_prompt(path, session_id).await;
        let Some(line) = input.read_line(&prompt)? else {
            break;
        };
        let command = line.trim();
        if command.is_empty() {
            continue;
        }
        input.remember(command);
        match submit_shell_command(path, command, session_id).await {
            Ok(ClientControl::Continue) => {}
            Ok(ClientControl::Exit) => break,
            Err(err) => eprintln!("Error: {err}"),
        }
        input.discard_pending_input()?;
    }
    Ok(())
}

async fn shell_prompt(path: &Path, session_id: Option<u64>) -> String {
    let request = session_id.map_or(IpcRequest::Health, |session_id| {
        IpcRequest::MarketSessionGet { session_id }
    });
    match ipc::request(path, &request, max_frame()).await {
        Ok(IpcResponse::Health(payload)) => format!("hl[{}]> ", payload.active),
        Ok(IpcResponse::MarketSession(session)) => format!("hl[{}]> ", session.active),
        _ => "hl[?]> ".to_string(),
    }
}

async fn submit_shell_command(
    path: &Path,
    command: &str,
    session_id: Option<u64>,
) -> anyhow::Result<ClientControl> {
    submit_and_print(path, command, session_id).await
}

async fn run_once(path: &Path, words: Vec<String>, session_id: Option<u64>) -> anyhow::Result<()> {
    let first = words
        .first()
        .map(|word| word.to_ascii_lowercase())
        .unwrap_or_default();
    match first.as_str() {
        "health" => {
            anyhow::ensure!(words.len() == 1, "health does not accept arguments");
            let request = session_id.map_or(IpcRequest::Health, |session_id| {
                IpcRequest::MarketSessionHealth { session_id }
            });
            print_health(ipc::request(path, &request, max_frame()).await?)
        }
        "metrics" => {
            anyhow::ensure!(words.len() == 1, "metrics does not accept arguments");
            print_response(ipc::request(path, &IpcRequest::Metrics, max_frame()).await?)
        }
        "command" => {
            anyhow::ensure!(
                words.len() == 3 && words[1].eq_ignore_ascii_case("get"),
                "command supports exactly: command get <id>"
            );
            let command_id = words[2].parse()?;
            print_response(
                ipc::request(path, &IpcRequest::CommandGet { command_id }, max_frame()).await?,
            )
        }
        "exec" => {
            anyhow::ensure!(words.len() > 1, "exec requires a command");
            let _ = submit_and_print(path, &words[1..].join(" "), session_id).await?;
            Ok(())
        }
        _ => {
            let _ = submit_and_print(path, &words.join(" "), session_id).await?;
            Ok(())
        }
    }
}

async fn submit_and_print(
    path: &Path,
    command: &str,
    session_id: Option<u64>,
) -> anyhow::Result<ClientControl> {
    let request = match session_id {
        Some(session_id) => IpcRequest::CommandSubmitScoped {
            session_id,
            command: command.to_string(),
        },
        None => IpcRequest::CommandSubmit {
            command: command.to_string(),
        },
    };
    let response = ipc::request(path, &request, max_frame()).await?;
    let command_id = match response {
        IpcResponse::CommandSubmit(payload) => payload.command_id,
        IpcResponse::Error { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected command submit response: {other:?}"),
    };
    loop {
        let response =
            ipc::request(path, &IpcRequest::CommandGet { command_id }, max_frame()).await?;
        match response {
            IpcResponse::CommandGet(record) => {
                if record.state == CommandExecutionState::Completed {
                    if let Some(output) = record.response {
                        return render_command_response(output);
                    }
                    anyhow::bail!("completed command {command_id} missing response");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            IpcResponse::Error { message } => anyhow::bail!(message),
            other => anyhow::bail!("unexpected command get response: {other:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientControl {
    Continue,
    Exit,
}

fn render_command_response(output: CommandResponse) -> anyhow::Result<ClientControl> {
    if output.clear {
        clear_terminal()?;
    }
    for line in output.lines {
        println!("{line}");
    }
    if !output.ok {
        anyhow::bail!("command failed");
    }
    Ok(if output.exit {
        ClientControl::Exit
    } else {
        ClientControl::Continue
    })
}

fn clear_terminal() -> io::Result<()> {
    print!("\x1b[2J\x1b[H");
    io::stdout().flush()
}

fn print_response(response: IpcResponse) -> anyhow::Result<()> {
    if let IpcResponse::Error { message } = &response {
        anyhow::bail!(message.clone());
    }
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn print_health(response: IpcResponse) -> anyhow::Result<()> {
    let healthy = matches!(&response, IpcResponse::Health(payload) if payload.ok);
    print_response(response)?;
    anyhow::ensure!(healthy, "daemon is not healthy");
    Ok(())
}

fn max_frame() -> usize {
    8 * 1024 * 1024
}

struct FillWatch {
    path: PathBuf,
    session_id: Option<u64>,
    last_seq: u64,
    error: Option<String>,
}

struct ShellInput {
    history: History,
    interactive: bool,
    fills: Option<FillWatch>,
    _raw: Option<RawMode>,
}

impl ShellInput {
    fn new(path: &Path, session_id: Option<u64>) -> anyhow::Result<Self> {
        let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
        let raw = interactive.then(RawMode::enable).transpose()?;
        let fills = if interactive {
            let initial = fetch_fills_frame(path, session_id, 0)?;
            Some(FillWatch {
                path: path.to_path_buf(),
                session_id,
                last_seq: initial.iter().map(|fill| fill.seq).max().unwrap_or(0),
                error: None,
            })
        } else {
            None
        };
        Ok(Self {
            history: History::default(),
            interactive,
            fills,
            _raw: raw,
        })
    }

    fn read_line(&mut self, prompt: &str) -> anyhow::Result<Option<String>> {
        if self.interactive {
            read_interactive(prompt, &mut self.history, self.fills.as_mut())
        } else {
            print!("{prompt}");
            io::stdout().flush()?;
            let mut line = String::new();
            if io::stdin().read_line(&mut line)? == 0 {
                Ok(None)
            } else {
                Ok(Some(line))
            }
        }
    }

    fn remember(&mut self, command: &str) {
        self.history.push(command.to_string());
    }

    fn discard_pending_input(&mut self) -> anyhow::Result<()> {
        if self.interactive {
            RawMode::drain_stdin()?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct History {
    items: Vec<String>,
    cursor: Option<usize>,
    draft: String,
}

impl History {
    fn push(&mut self, command: String) {
        if self.items.last() != Some(&command) {
            self.items.push(command);
        }
        self.cursor = None;
        self.draft.clear();
    }

    fn older(&mut self, current: &str) -> Option<&str> {
        if self.items.is_empty() {
            return None;
        }
        let next = match self.cursor {
            Some(0) => 0,
            Some(index) => index - 1,
            None => {
                self.draft = current.to_string();
                self.items.len() - 1
            }
        };
        self.cursor = Some(next);
        Some(&self.items[next])
    }

    fn newer(&mut self) -> Option<&str> {
        let cursor = self.cursor?;
        if cursor + 1 < self.items.len() {
            let next = cursor + 1;
            self.cursor = Some(next);
            Some(&self.items[next])
        } else {
            self.cursor = None;
            Some(&self.draft)
        }
    }

    fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }
}

struct LineEditor<'a> {
    history: &'a mut History,
    line: String,
    cursor: usize,
    interrupt_armed: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum InputAction {
    Continue,
    Redraw,
    Submit(String),
    Exit,
}

enum InputKey {
    Enter,
    CtrlC,
    CtrlD,
    Backspace,
    Escape(Escape),
    Char(char),
    Other,
}

impl<'a> LineEditor<'a> {
    fn new(history: &'a mut History) -> Self {
        Self {
            history,
            line: String::new(),
            cursor: 0,
            interrupt_armed: false,
        }
    }

    fn line(&self) -> &str {
        &self.line
    }

    fn cursor(&self) -> usize {
        self.cursor
    }

    fn apply(&mut self, key: InputKey) -> InputAction {
        match key {
            InputKey::Enter => InputAction::Submit(std::mem::take(&mut self.line)),
            InputKey::CtrlC => {
                if self.line.is_empty() && self.interrupt_armed {
                    return InputAction::Exit;
                }
                self.line.clear();
                self.cursor = 0;
                self.interrupt_armed = true;
                self.history.reset_navigation();
                InputAction::Redraw
            }
            InputKey::CtrlD if self.line.is_empty() => InputAction::Exit,
            InputKey::CtrlD | InputKey::Other => {
                self.interrupt_armed = false;
                InputAction::Continue
            }
            InputKey::Backspace => {
                self.interrupt_armed = false;
                if self.cursor == 0 {
                    return InputAction::Continue;
                }
                self.cursor -= 1;
                self.line.remove(self.cursor);
                self.history.reset_navigation();
                InputAction::Redraw
            }
            InputKey::Escape(Escape::Up) => {
                self.interrupt_armed = false;
                if let Some(value) = self.history.older(&self.line) {
                    self.line.clear();
                    self.line.push_str(value);
                    self.cursor = self.line.len();
                    InputAction::Redraw
                } else {
                    InputAction::Continue
                }
            }
            InputKey::Escape(Escape::Down) => {
                self.interrupt_armed = false;
                if let Some(value) = self.history.newer() {
                    self.line.clear();
                    self.line.push_str(value);
                    self.cursor = self.line.len();
                    InputAction::Redraw
                } else {
                    InputAction::Continue
                }
            }
            InputKey::Escape(Escape::Left) => {
                self.interrupt_armed = false;
                if self.cursor > 0 {
                    self.cursor -= 1;
                    InputAction::Redraw
                } else {
                    InputAction::Continue
                }
            }
            InputKey::Escape(Escape::Right) => {
                self.interrupt_armed = false;
                if self.cursor < self.line.len() {
                    self.cursor += 1;
                    InputAction::Redraw
                } else {
                    InputAction::Continue
                }
            }
            InputKey::Escape(Escape::Other) => {
                self.interrupt_armed = false;
                InputAction::Continue
            }
            InputKey::Char(ch) => {
                self.interrupt_armed = false;
                self.line.insert(self.cursor, ch);
                self.cursor += 1;
                self.history.reset_navigation();
                InputAction::Redraw
            }
        }
    }
}

fn read_interactive(
    prompt: &str,
    history: &mut History,
    mut fills: Option<&mut FillWatch>,
) -> anyhow::Result<Option<String>> {
    let mut input = io::stdin();
    let mut out = io::stdout();
    let mut editor = LineEditor::new(history);
    write!(out, "{prompt}")?;
    out.flush()?;
    loop {
        let mut byte = [0_u8; 1];
        if input.read(&mut byte)? == 0 {
            if let Some(fills) = fills.as_deref_mut() {
                announce_fills(&mut out, prompt, editor.line(), editor.cursor(), fills)?;
            }
            continue;
        }
        let key = match byte[0] {
            b'\r' | b'\n' => InputKey::Enter,
            3 => InputKey::CtrlC,
            4 => InputKey::CtrlD,
            8 | 127 => InputKey::Backspace,
            27 => InputKey::Escape(read_escape(&mut input)?),
            byte if (0x20..=0x7e).contains(&byte) => InputKey::Char(byte as char),
            _ => InputKey::Other,
        };
        match editor.apply(key) {
            InputAction::Continue => {}
            InputAction::Redraw => redraw(&mut out, prompt, editor.line(), editor.cursor())?,
            InputAction::Submit(line) => {
                writeln!(out)?;
                return Ok(Some(line));
            }
            InputAction::Exit => {
                writeln!(out)?;
                return Ok(None);
            }
        }
    }
}

fn announce_fills(
    out: &mut io::Stdout,
    prompt: &str,
    line: &str,
    cursor: usize,
    fills: &mut FillWatch,
) -> anyhow::Result<()> {
    let new = match fetch_fills_frame(&fills.path, fills.session_id, fills.last_seq) {
        Ok(new) => {
            if fills.error.take().is_some() {
                write!(out, "\r\x1b[2Kfill feed reconnected\r\n")?;
                redraw(out, prompt, line, cursor)?;
            }
            new
        }
        Err(err) => {
            let message = err.to_string();
            if fills.error.as_deref() != Some(&message) {
                fills.error = Some(message.clone());
                write!(out, "\r\x1b[2Kfill feed error: {message}\r\n")?;
                redraw(out, prompt, line, cursor)?;
            }
            return Ok(());
        }
    };
    let Some(max_seq) = new.iter().map(|fill| fill.seq).max() else {
        return Ok(());
    };
    fills.last_seq = max_seq;
    write!(out, "\r\x1b[2K")?;
    for fill in &new {
        write!(out, "{}\r\n", format_fill(fill))?;
    }
    redraw(out, prompt, line, cursor)?;
    Ok(())
}

fn fetch_fills_frame(
    path: &Path,
    session_id: Option<u64>,
    after_seq: u64,
) -> anyhow::Result<Vec<Fill>> {
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = rmp_serde::to_vec_named(&match session_id {
        Some(session_id) => IpcRequest::FillsScoped {
            session_id,
            after_seq,
        },
        None => IpcRequest::Fills { after_seq },
    })?;
    stream.write_all(&u32::try_from(request.len())?.to_be_bytes())?;
    stream.write_all(&request)?;
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    anyhow::ensure!(len > 0 && len <= max_frame(), "ipc frame length {len}");
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload)?;
    match rmp_serde::from_slice::<IpcResponse>(&payload)? {
        IpcResponse::Fills(payload) => Ok(payload.fills),
        IpcResponse::Error { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected fills response: {other:?}"),
    }
}

fn format_fill(fill: &Fill) -> String {
    let side = if fill.is_buy { "buy" } else { "sell" };
    let pnl = fill.closed_pnl.to_string();
    let pnl = if pnl.starts_with('-') {
        pnl
    } else {
        format!("+{pnl}")
    };
    format!(
        "fill {} {} {} @ {} pnl={}",
        fill.symbol, side, fill.size, fill.price, pnl
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    Up,
    Down,
    Left,
    Right,
    Other,
}

fn read_escape(input: &mut io::Stdin) -> io::Result<Escape> {
    let mut bytes = [0_u8; 2];
    if input.read(&mut bytes[..1])? == 0 {
        return Ok(Escape::Other);
    }
    if input.read(&mut bytes[1..2])? == 0 {
        return Ok(Escape::Other);
    }
    Ok(match bytes {
        [b'[', b'A'] | [b'O', b'A'] => Escape::Up,
        [b'[', b'B'] | [b'O', b'B'] => Escape::Down,
        [b'[', b'C'] | [b'O', b'C'] => Escape::Right,
        [b'[', b'D'] | [b'O', b'D'] => Escape::Left,
        _ => Escape::Other,
    })
}

fn redraw(out: &mut io::Stdout, prompt: &str, line: &str, cursor: usize) -> io::Result<()> {
    write!(out, "\r\x1b[2K{prompt}{line}")?;
    let tail = line.len().saturating_sub(cursor);
    if tail > 0 {
        write!(out, "\x1b[{tail}D")?;
    }
    out.flush()
}

struct RawMode {
    saved: Termios,
}

impl RawMode {
    fn enable() -> anyhow::Result<Self> {
        let stdin = io::stdin();
        let saved = tcgetattr(&stdin)
            .map_err(|err| anyhow::anyhow!("failed to read terminal state: {err}"))?;
        let mut raw = saved.clone();
        raw.local_modes
            .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG);
        raw.special_codes[SpecialCodeIndex::VMIN] = 0;
        raw.special_codes[SpecialCodeIndex::VTIME] = 5;
        tcsetattr(&stdin, OptionalActions::Now, &raw).map_err(|err| {
            anyhow::anyhow!("failed to enter noncanonical terminal mode for shell input: {err}")
        })?;
        Ok(Self { saved })
    }

    fn drain_stdin() -> anyhow::Result<()> {
        Self::set_timeout(0)?;
        let result = drain_stdin();
        let restore = Self::set_timeout(5);
        match (result, restore) {
            (Err(err), _) => Err(err),
            (Ok(()), Err(err)) => Err(err),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn set_timeout(value: u8) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let mut termios = tcgetattr(&stdin)
            .map_err(|err| anyhow::anyhow!("failed to read terminal state: {err}"))?;
        termios.special_codes[SpecialCodeIndex::VMIN] = 0;
        termios.special_codes[SpecialCodeIndex::VTIME] = value;
        tcsetattr(&stdin, OptionalActions::Now, &termios)
            .map_err(|err| anyhow::anyhow!("failed to update raw terminal timeout: {err}"))?;
        Ok(())
    }
}

fn drain_stdin() -> anyhow::Result<()> {
    let mut input = io::stdin();
    let mut buf = [0_u8; 64];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(io::stdin(), OptionalActions::Now, &self.saved);
    }
}
