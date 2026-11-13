<!-- Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): updated project overview, removed hl-grid/HTTP references, and documented --no-hip3 and hl --json. -->
# hyperdesk

An unofficial command-line and local-IPC trading client for Hyperliquid, adapted from upstream
`bahmgit/hlcli`. One resident `hld` daemon owns credentials, exchange connections, state,
readiness/risk checks, signing, and journaled execution over a private Unix domain socket.
Thin `hl` clients and local automation share that daemon, including independent market-scoped
sessions and a deterministic `hl --json` machine envelope.

This software can submit real orders and is provided without warranty. Start on Testnet with a
dedicated delegated API (Agent) wallet and limited funded exposure. Never supply your Master Wallet
private key (`hld` rejects Master/Vault signer collisions before writing keystore files).

## Requirements

- A Unix-like system with Rust installed through `rustup`. `rust-toolchain.toml` pins Rust `1.91.1`.
- A Hyperliquid account and an approved delegated API (Agent) wallet for signed operation.

## Build

```bash
cargo build --locked --release --bins
```

## Testnet quickstart

Use a dedicated `0700` data directory for Testnet credentials and state:

```bash
export HL_V2_DATA_DIR="$HOME/.hyperdesk-testnet"
install -d -m 700 "$HL_V2_DATA_DIR"
cp examples/hl-v2.testnet.json "$HL_V2_DATA_DIR/hl-v2.json"
target/release/hld --testnet
```

By default (`"includeHip3": true`, `"allowedSymbols": []`), `hld` discovers all native Perp, native
Spot, and HIP-3 builder-DEX markets. For faster native-only startup when your account holds zero
HIP-3 positions, margin, or open orders, pass `--no-hip3` (or `"includeHip3": false`).

On first signed start, `hld` prompts on `/dev/tty` for the main account address, delegated
API-wallet private key, and a new keystore password. Subsequent starts unlock the stored profile.
In a second terminal:

```bash
export HL_V2_DATA_DIR="$HOME/.hyperdesk-testnet"
target/release/hl doctor
target/release/hl
```

Use `help`, `help <category|command>`, or `help all` in the interactive shell. One-shot human and
machine (`--json`) commands use the same typed grammar:

```bash
target/release/hl status
target/release/hl --market BTC orders
target/release/hl --json health
target/release/hl --market ETH --json status
```

Run at most one signing `hld` daemon per API-wallet key.

## Documentation

- [Operations](docs/operations.md): configuration, credentials, socket/KDF hardening, recovery, troubleshooting.
- [Commands](docs/commands.md): command families, sizing, `hl --json` contract, exit codes, market scope.
- [Architecture](docs/architecture.md): module ownership, execution, reconciliation, persistence, trust boundaries.
- [Validation](docs/validation.md): deterministic & security gate, dependency audit notes, bounded Testnet acceptance.
- [Security](SECURITY.md) and [contribution policy](CONTRIBUTING.md).

The exchange protocol can change independently. Consult the
[official API documentation](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api)
and recertify against the target network before meaningful use.

Licensed under [Apache-2.0](LICENSE); attribution and modification notices are in [NOTICE](NOTICE).
