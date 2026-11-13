<!-- Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): updated deterministic/security gates, documented dependency upgrades and RUSTSEC-2026-0387/reqwest-tower paths, and updated Testnet acceptance. -->
# Validation

## Deterministic & security gate

Run on `rustc 1.91.1` (`rust-toolchain.toml`) before every commit/handoff:

```bash
cargo fmt --all -- --check
cargo check --workspace --locked --all-targets
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked --all-targets
cargo build --locked --release --bins
test -z "$(cargo tree --locked --all-targets -i axum 2>/dev/null)"
! rg -n '\bmod server\b|server::|Command::new\("stty"\)' src/
cargo tree --locked -e features --target all -i rkyv
cargo audit --no-fetch -D unsound \
  --ignore RUSTSEC-2026-0235 \
  --ignore RUSTSEC-2026-0387
git diff --check
```

Tests require no exchange account, live network, or secrets. They exercise compilation under
`#![forbid(unsafe_code)]`, mocked exchange HTTP/WebSocket boundaries, journal-before-post and
`/info` `orderStatus` + `userFills` VWAP reconciliation, `marketCrossBps` propagation, `--no-hip3`
startup/live exposure guards, Agent-vs-Master/Vault collision rejection, `0700`/`0600` UDS +
`peer_cred()` + `64 KiB`/`256` queue limits, Argon2id floors, `5s`/`15s` HTTP timeouts, PTY raw-mode
restoration, and the `hl --json` (`0`/`1`/`2`) CLI contract.

## Dependencies and RustSec audit posture

Patched crates compiled in `Cargo.lock`:

- TLS & X.509 roots: `rustls 0.23.46`, `rustls-webpki 0.103.17`, `webpki-roots 1.0.12`,
  `webpki-root-certs 1.0.12` (resolves `RUSTSEC-2026-0346` through `RUSTSEC-2026-0349`).
- Async runtime & syscalls: `tokio 1.54.1` (`>= 1.53.3`, resolves `RUSTSEC-2026-0386`),
  `rustix 1.1.6` (`>= 1.1.6`, resolves `RUSTSEC-2026-0486`).
- Lockfile cleanup: `quinn-proto` upgraded to `0.11.19` (eliminates `RUSTSEC-2026-0338` through
  `RUSTSEC-2026-0342` from `Cargo.lock` entirely; `cargo tree --locked -e features --target all -i quinn-proto`
  also confirms it is uncompiled).
- HTTP server removal: `axum` and direct `tower` dependencies were removed from `Cargo.toml` along
  with `src/server.rs` (`cargo tree --locked --all-targets -i axum` matches no package). Note that
  `reqwest 0.13.4` internally depends on `tower 0.5.3` and `tower-http 0.6.11` (`follow-redirect`,
  `retry`, `timeout`) strictly as outbound HTTP client middleware (`hl-v2 -> reqwest 0.13.4 -> tower-http 0.6.11 -> tower 0.5.3`);
  zero HTTP server listeners exist in `hld`.

Every `--ignore` flag in the `cargo audit` gate is verified and bounded:

1. `RUSTSEC-2026-0235` (`rkyv 0.7.46`, optional dependency of `rust_decimal`): **not compiled** —
   `cargo tree --locked -e features --target all -i rkyv` prints `warning: nothing to print.`
2. `RUSTSEC-2026-0387` (`crypto-bigint 0.5.5`, patched only in breaking `>= 0.7.0-rc.15`; `alloy*`
   `2.1.1`/`1.7.3` on `rustc 1.91.1` remains on the `elliptic-curve 0.13.8` / `k256 0.13.4` /
   `ecdsa 0.16.9` line): compiled path is
   `crypto-bigint 0.5.5 -> elliptic-curve 0.13.8 / k256 0.13.4 -> ecdsa 0.16.9 -> alloy-signer 2.1.1 / alloy-signer-local 2.1.1 -> hl-v2 0.1.0`.
   The advisory affects variable-time / `NonZero::new` behavior in modular inversion / Bernstein-Yang
   GCD. In `hld`, `k256` runs exclusively in-process for deterministic RFC 6979 secp256k1 ECDSA
   signing over fixed curve parameters and locally constructed EIP-712 / L1 hashes, never on
   attacker-chosen moduli or over a network-exposed signing oracle. Upgrade `alloy*` / `k256` once a
   stable release adopts `crypto-bigint >= 0.7`.

## Bounded live acceptance (Pre-Mainnet Testnet gate)

Before connecting any funded Mainnet Agent Wallet, execute bounded live acceptance on a dedicated
**Hyperliquid Testnet** account with tiny sizes and keep operational logs outside the repository:

1. Pass the deterministic & security gate above and build `target/release/{hld,hl}`.
2. Provision a Testnet account and register a delegated **Agent Wallet** (`approveAgent`). Confirm in
   a throwaway directory that pasting the Master Wallet address + Master key is rejected by `hld`
   (`R-07`) without creating `credentials.bin`. Then initialize the real Testnet Agent Wallet profile
   and confirm `0700` data/runtime directories and `0600` `credentials.bin`, `salt.bin`, and
   `runtime/backend.sock` (testing both default `includeHip3: true` startup and `--no-hip3` startup).
3. Verify read & agent JSON envelopes: `hl --json health`, `hl --json balances`,
   `hl --market BTC --json status`, and `hl --json markets` (`exit 0`, single JSON object on `stdout`).
4. Exercise resting post-only limit placement, `move`, `resize`, `hl --json orders`, and `cancel`.
5. Exercise a tiny IOC market trade with custom `marketCrossBps`, then inspect
   `hl --json command get <commandId>` and `hl --json position`.
6. Exercise protected entries (`buy <size> sl <px> tp <px>`, a scaled/partial-fill case, and a native
   `trail` case), restart `hld` while brackets are live on Testnet, unlock, and verify clean
   `ActionJournal` + `InfoReconciler` + WebSocket convergence without duplicate orders or lost exits.
7. Exercise `chase` and conditional `twap` start/cancel, `close` any remaining Testnet position, and
   confirm `hl --json orders` and `hl --json position` are flat with zero unresolved journal rows.

### Public read-only discovery acceptance

Run the optional public read-only discovery check explicitly when network access is available:

```bash
cargo test --locked --test discovery_live -- --ignored --nocapture
```

It initializes the full market universe and per-DEX `openOrders` snapshots on Mainnet and Testnet for
the zero address without signing or submitting actions.
