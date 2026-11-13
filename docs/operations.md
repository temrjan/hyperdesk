<!-- Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): removed HTTP server/keybind/hl-grid sections and documented strict config, --no-hip3, signer collision, UDS, and KDF hardening. -->
# Operations

## Processes and data

Run at most one signing `hld` per delegated API-wallet key and any number of local `hl` clients
against it. The default data directory is `~/.hyperliquid-cli`; override with `--data-dir` or
`HL_V2_DATA_DIR`. Keep it private (`0700`), outside the source tree, and use separate directories
for Testnet and Mainnet.

| Path within data directory | Contents |
| --- | --- |
| `hl-v2.json` | Strict operator configuration (`#[serde(deny_unknown_fields)]`) |
| `wallet_profiles.json` | Profile index and account metadata (`0600`) |
| `wallets/<profile>/credentials.bin`, `salt.bin` | Encrypted credentials and Argon2id salt (`0600`) |
| `runtime/backend.sock` | Local Unix domain control socket (`0600` inside `0700` `runtime/`) |
| `runtime/execution_journal_v2.jsonl` | Durable action and recovery evidence (`0600`) |
| `runtime/managed_state_v2.json` | Persisted chase/protection state |

Back up sensitive data only through an encrypted channel. Existing profile directories are reused;
starting a release binary against one is not an isolated test.

## Configuration

`hld` and `hl` read `<data-dir>/hl-v2.json`. Malformed JSON, invalid values, and **any unknown or
removed field** (including legacy `bind`, `allowRemote`, or `backendToken`) fail loading immediately.
`hld` also re-validates the effective configuration after applying CLI flags (`--mainnet`,
`--testnet`, `--no-hip3`).

| JSON field | Meaning | Default |
| --- | --- | --- |
| `network` | `Mainnet` or `Testnet` | `Mainnet` |
| `defaultSymbol` | Initial canonical market | `BTC` |
| `allowedSymbols` | Canonical market allowlist; empty (`[]`) permits all discovered markets | `[]` |
| `includeHip3` | Load HIP-3 builder-DEX perpetuals alongside native Perps and Spot | `true` |
| `symbolAliases` | Alias-to-canonical map; requires nonempty `allowedSymbols` and allowed targets | `{}` |
| `vaultAddress` | Authorized vault/subaccount address to trade for | unset |
| `requiredAccountMode` | `standard`, `unifiedAccount`, `portfolioMargin`, or `any` | `any` |
| `marketCrossBps` | IOC market/trigger crossing allowance in basis points (`0`–`10000`), applied across both direct and `ManagedExecutor` orders | `50` |

`examples/hl-v2.testnet.json` enables the full market universe (`"allowedSymbols": []`,
`"includeHip3": true`) on Testnet. Without a config file or overrides, `hld` defaults to Mainnet
with unrestricted native Perp, Spot, and HIP-3 market discovery.

Environment overrides: `HL_V2_NETWORK`, `HL_V2_DEFAULT_SYMBOL`, `HL_V2_ALLOWED_SYMBOLS`
(comma-separated), `HL_V2_INCLUDE_HIP3` (`true`/`false`/`1`/`0`/`yes`/`no`/`on`/`off`),
`HL_V2_VAULT_ADDRESS`, `HL_V2_REQUIRED_ACCOUNT_MODE`, and `HL_V2_MARKET_CROSS_BPS`.

### Guarded `--no-hip3` (`includeHip3: false`) opt-out

When `includeHip3` is `false` (via `"includeHip3": false`, `HL_V2_INCLUDE_HIP3=false`, or
`hld --no-hip3`):

1. `Config::validate()` rejects `:`-prefixed HIP-3 symbols in `defaultSymbol` or `allowedSymbols`.
2. `InfoClient` skips builder-DEX market metadata and per-DEX `openOrders`/`twapStates` subscriptions,
   reducing startup WebSocket snapshot fan-out on native-only accounts.
3. Startup (`seed_state` when a user is configured) subscribes to `allDexsClearinghouseState` on the
   startup WebSocket and **fails closed** if any non-root DEX has a non-zero position, margin usage
   (`totalMarginUsed > 0`), or (when `accountValue > 0`) open orders in `frontendOpenOrders`.
4. Live streaming keeps `allDexsClearinghouseState` and `userFills` active; if non-root DEX positions,
   margin usage, or fills appear mid-session while `includeHip3=false`, `hld` sets a persistent
   health error (`HealthPayload.ok = false`, surfaced in `hl health` and `hl doctor`) instructing the
   operator to restart without `--no-hip3`.

## Credentials, keystore floors, and IPC security

Use an approved delegated **API (Agent) wallet**, never the Master account or Vault private key.
Both profile creation and startup unlock call `validate_agent_signer` to verify that the derived
signer address differs from `main_wallet` and `vault_address` before writing or using keystore files.

```bash
target/release/hld --testnet --wallet-profile default
```

Interactive password prompts open `/dev/tty` directly, disable echo via `rustix::termios` inside a
`Drop` guard, read into `Zeroizing<String>`, and strip only trailing `\r`/`\n` (preserving intentional
spaces). Unattended services may pass `--password-file` (`HL_V2_PASSWORD_FILE`), which must be a
regular file (not a symlink/FIFO) with mode `0600` (`(mode & 0o077) == 0`). Keystore key derivation
unconditionally enforces Argon2id floors of `HL_KDF_MEM_KiB >= 19456` (`19 MiB`),
`HL_KDF_ITERS >= 2`, and `lanes == 1`.

Before binding `backend.sock` (or a custom `--ipc-path`), `ipc::bind` ensures the parent directory is
a real (non-symlink) directory owned by `geteuid()` with mode `0700` (rejecting group/world-accessible
directories such as `/tmp`), binds the socket with mode `0600`, and `serve_bound` verifies
`stream.peer_cred()?.uid() == geteuid()` on every client connection.

For read-only operation without a private key:

```bash
target/release/hld --testnet --read-only --user 0x0000000000000000000000000000000000000000
```

## Multiple market terminals and agents

```bash
target/release/hl --market BTC
target/release/hl --market ETH
target/release/hl --market ETH --json status
```

Use the same `--data-dir` / `HL_V2_DATA_DIR` or `--ipc-path` / `HL_V2_IPC_PATH` across `hld` and
`hl`. Each `--market <SYMBOL>` session scopes its market selection without changing global
`state.active`.

## Upgrading from local trailing

Managed-state schema v3 removes local trailing monitors. Startup refuses v1/v2 state containing an
active local trail or an unfinished trailing attachment. Safe nontrailing v2 work (chases and fixed
attached protection) is retained.

## Recovery and troubleshooting

- Unhealthy/stale state: inspect `hl --json health`, `hl doctor`, connections, account mode, and
  readiness reasons (including any untracked HIP-3 warning if running with `--no-hip3`).
- `hl --json` exit code `2` (`outcomeUnknown: true`): **do not blindly resubmit**; check
  `hl --json command get <commandId>`, `hl --json orders`, and `hl --json position`.
- IPC errors: verify that `hl` and `hld` run under the same OS UID and point to the same `0700`
  runtime directory / `0600` socket.
- Ambiguous delivery or execution halt: stop submitting, preserve `execution_journal_v2.jsonl`, and
  reconcile positions, orders, and TWAPs against Hyperliquid before restarting.
- Suspected key exposure: stop `hld`, revoke the delegated API wallet on Hyperliquid, and provision a
  new API wallet. See `SECURITY.md`.

### Discovery and read rate limits

Both networks discover perpetuals with `perpDexs` and `allPerpMetas`, preserving exchange DEX and
asset indices. Initialization collects identity-checked `openOrders` WebSocket snapshots for every
configured DEX before publishing state. REST `InfoClient` requests enforce a `5s` connect timeout and
`15s` request timeout and share a process-local rolling budget of `1000` weight per minute, with
bounded `429` cooldown retries (`<= 2` retries when `Retry-After <= 120s`).
