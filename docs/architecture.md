<!-- Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): removed HTTP server and operator DSL modules, documented UDS/queue/signer hardening, VWAP reconciliation, and HIP-3 exposure guards. -->
# Architecture

`hld` owns exchange connectivity, signing, account state, readiness/risk, and durable execution.
`hl` owns terminal interaction (`rustix::termios` line editor or `--json` one-shot envelope) and sends
typed local IPC requests over a private Unix domain socket. All clients share one global execution
order; market-scoped sessions retain independent market selection. All production targets enforce
`#![forbid(unsafe_code)]`.

## Modules

| Modules | Responsibility |
| --- | --- |
| `command`, `catalog` | Typed grammar, aliases, help |
| `runtime`, `scope` | Bounded submission queue (`64 KiB` / `256` depth), confirmations, market sessions |
| `planner` | State validation, sizing, precision, `marketCrossBps`, intent-to-action planning |
| `managed` | Chase and unfinished dependent entry protection, persisted work |
| `core`, `execution` | Serialized execution, journal, acknowledgements, `/info` `orderStatus` + `userFills` VWAP reconciliation |
| `protocol`, `exchange` | Wire types, EIP-712 / L1 signatures, action WebSocket transport |
| `info`, `info_reads`, `feed`, `state` | Metadata, HIP-3 startup/live exposure guards, bounded HTTP/WS reads, freshness, readiness |
| `security`, `config` | Encrypted keystore, Agent-vs-Master/Vault collision guard, Argon2id floors, strict configuration |
| `ipc`, `metrics` | `0700`-dir / `0600`-socket local IPC with `peer_cred()` UID verification, counters |

## Execution and reconciliation

Parse a typed command, validate against the relevant state, and enter the daemon's global execution
queue. Persist a pending journal row containing reconciliation context, sign with the signer-owned
nonce, and post over the action WebSocket. Classify the response as accepted, rejected, or ambiguous;
append its terminal outcome (including acknowledged identities) and repair the typed read model from
the receipt.

Normal submit-to-ack performs no hidden REST, sleep, refresh, parsing, or UI/cache work. Info refresh
and reconciliation are explicit or background work. When a WebSocket acknowledgement times out or
restarts require reconciliation, `InfoReconciler` queries authoritative `/info` `orderStatus`
(`origSz` and remaining `sz` to preserve partial and full fills) and issues a single batch
`userFills` query whenever reconciled orders have `filled_sz > 0`, computing exact `Decimal` fill
VWAPs (`sum(px * sz) / sum(sz)`) rather than polluting `entry_price` with IOC `limitPx`. If no fill
rows are available yet, `apply_filled_position` still advances `position.size` so dependent `tp`/`sl`
/ `trail` protection arms immediately, while setting `entry_price = None` until the next
`clearinghouseState` snapshot. Zero-fill terminal cancels preserve `oid` in `RecoveredOrderOutcome`.
An outcome that may have reached the exchange and remains unresolved halts execution rather than
being retried blindly.

## State and managed work

One `TradingState` combines subscriptions and authoritative snapshots with timestamps and explicit
known/fresh flags. Trading validates relevant market, account, order, book, capacity, and position
state. Position reconciliation uses the clearinghouse snapshot time when supplied, so an older
snapshot cannot erase a newer acknowledged fill. Fill receipts use the position captured before
submission, never a delta applied to a concurrently updated cache. A concurrent snapshot agreeing
with the expected position is retained with its authoritative details; a conflicting snapshot
invalidates position readiness until a fresh snapshot arrives. Spot inventory is represented by
balances, never perp positions. Application ping/pong bounds feed liveness even when books are quiet;
a missed pong retires the connection.

By default (`includeHip3: true`), `InfoClient` and `feed` load all native Perp, native Spot, and
HIP-3 builder-DEX markets. `feed` always subscribes to `allDexsClearinghouseState` when a user is
configured. When `--no-hip3` (`includeHip3: false`) is selected, startup inspects
`allDexsClearinghouseState` (and `frontendOpenOrders` for any non-root DEX with positive account
value) and fails closed if any non-root DEX carries positions, margin usage, or open orders; if
non-root DEX positions, margin usage, or fills appear later on the live WebSocket feed, `TradingState`
records a persistent untracked-HIP-3 health failure (`HealthPayload.ok = false`).

Managed chase and unfinished attached protection persist their intent, exact order identities, and
configured `marketCrossBps` across all six planning sites. Only fills attributable to owned entry
orders can arm or resize protection. Operator cancellation and managed transitions share a lock.
Fixed TP/SL protection grows with owned fills; trailing protection adds an independent native order
per new fill increment, retaining earlier watermarks. If a native tranche ends before attachment work
finishes, the daemon cancels the unfilled entry and retains surviving exits for the remaining
position. Unresolved delivery halts execution instead of attempting cancellation or retry.

Native trails are reduce-only exchange orders driven by mark prices. Hyperliquid owns activation,
watermarks, and execution; the daemon has no local ratchet or active-trail database. Startup queries
open orders across configured DEXs, adopting native trails placed through any interface. Closing a
client does not stop exchange orders or persisted managed work.

Fast cancel is selected only for a whole batch of fresh known non-trigger orders without children.
Mixed, protective, stale, or unknown targets use ordinary cancel. The flag is omitted entirely when
false, including from the signed MessagePack encoding.

## Trust boundaries

- `hld` exposes **zero** TCP/HTTP listeners; all local control flows over a `0600` Unix domain socket
  inside a real (non-symlink) `0700` directory owned by `geteuid()`, and `serve_bound` verifies
  `stream.peer_cred()?.uid() == geteuid()` on every accepted connection.
- `CommandQueue` rejects command payloads above `64 KiB` (`65,536` bytes) and rejects new submissions
  when `256` commands are queued/running, releasing retained scoped session job counts on rejection.
- Client/CLI code never holds private keys or signs exchange actions directly.
- Both wallet setup and startup unlock validate `signer.address()` against `main_wallet` and
  `vault_address`, refusing Master or Vault private keys before creating or using keystore files.
- Interactive passwords use `/dev/tty` with `rustix::termios` (`Drop`-restored echo guard) and
  `Zeroizing<String>` buffers; `--password-file` requires a regular `0600` file; Argon2id floors
  (`m_cost >= 19,456 KiB`, `t_cost >= 2`, `p_cost == 1`) are enforced unconditionally.
- Credential encryption does not protect a compromised running process or exposed password.
- Journals, profiles, configurations, and logs are sensitive operational records.

## Independent trading clients

`crates/hl-wire` defines the versioned `trading` IPC envelope for independent trading clients.
It contains only DTOs and exact decimal serialization, without signing or exchange dependencies.
Hello returns protocol version, daemon instance, network, account, read-only status, and market
metadata. Trading sessions have fixed markets. Identified submissions name that instance, session,
exact market, and a positive consecutive session sequence. A retry with identical contents returns
the same command ID; changed contents or retired sequences reject without execution. New submissions
reject when the global queue has `256` queued/running commands.

Daemon instances change on restart. The new API rejects old-instance submissions and result queries;
there is no cross-restart retry guarantee. Clients (including `hl --json`, which exits `2` with
`outcomeUnknown: true` when submission delivery or polling is ambiguous) must inspect `command get`,
`orders`, and `position` rather than blindly resubmitting.

Startup metadata uses aggregate perpetual metadata with DEX-index/namespace validation. Complete
per-DEX open-order snapshots are collected over a temporary read-only WebSocket before publishing
the initialized state; missing snapshots cannot be treated as empty. REST `InfoClient` instances
enforce a `5s` connect timeout and `15s` total request timeout and share a network-specific rolling
read budget within the process, with bounded `429` retries.
