<!-- Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): removed keybinds, @variables, and command chains, and documented deterministic hl --json mode. -->
# Commands

The daemon owns one typed command grammar for interactive shells, one-shot CLI calls, `hl --json`
agent invocations, and market-scoped sessions. `help all` is the exhaustive version-matched
catalogue; use `help <command>` for details. Examples are syntax illustrations, not order
recommendations.

## Client forms

```bash
hl                                      # interactive shell (rustix::termios raw mode on TTY)
hl status                               # one-shot human-readable command
hl exec buy '20$' at 40000              # explicit one-shot form
hl --market BTC                         # independent market-scoped shell
hl --market BTC orders                  # market-scoped one-shot read
hl health                               # JSON health, nonzero exit if unhealthy
hl metrics                              # daemon counters
hl command get 42                       # retained command result
hl --json health                        # single-object JSON envelope (exit 0 / 1 / 2)
hl --market ETH --json status           # market-scoped JSON read
hl --wait-ms 15000 --json buy 0.01      # duplicate-safe JSON execution with bounded wait
```

In an external shell, quote tokens containing `$` (such as `'20$'` or `'$entry-100'`) to prevent
shell variable expansion. Inside the interactive `hl` shell, enter them literally. Without
`--market`, `orders` and `position` are account-wide and `instrument` changes the global default
market. With `--market <SYMBOL>`, reads and trading target that session's market without mutating
global `state.active`. Portfolio and balances remain account-wide; spot inventory appears in
balances.

## Deterministic JSON mode (`hl --json`)

Pass `--json` (with optional `--wait-ms <u64>`, default `30000`, env `HL_V2_WAIT_MS`) for scripts
and AI agents. Every `hl --json <command...>` call writes **exactly one** `\n`-terminated JSON
object to `stdout` (zero ANSI escape codes or plain-text prefixes) and exits with a strict code:

| Exit code | Meaning | Agent / automation rule |
| --- | --- | --- |
| `0` | `"ok": true` — read query succeeded (and daemon is healthy for `health`), or queued command completed with zero `ambiguous` receipts | Safe completion |
| `1` | `"ok": false`, `"outcomeUnknown": false` — pre-enqueue validation/connect error (`"submitted": false, "retrySafe": true`), unhealthy `health`, or deterministic command rejection (`"submitted": true, "retrySafe": false`) | Inspect `"error"` before deciding |
| `2` | `"ok": false`, `"outcomeUnknown": true`, `"retrySafe": false` — command was accepted by (or written to) `hld`, then polling timed out (`--wait-ms`), socket closed, or exchange receipt was `ambiguous` | **MUST NOT blindly resubmit**; inspect `hl --json command get <commandId>`, `hl --json orders`, and `hl --json position` |

Structured read queries routed directly without queue mutation: `health`, `status` (`st`), `orders`,
`position` (`positions`/`pos`), `balances` (`portfolio`), `markets` (no filter args), `fills`,
`metrics`, and `command get <id>`. When `--market <SYMBOL>` is supplied, `health`, `status`,
`orders`, `position`, and `fills` are scoped to `<SYMBOL>`. All other commands submit via
`CommandSubmit` / `CommandSubmitScoped` and poll `CommandGet` every `50ms` up to `--wait-ms`.

## Families

| Family | Commands/aliases | Purpose |
| --- | --- | --- |
| Read | `status`/`st`/`refresh`/`r` | Refresh active state and show readiness, orders, TWAPs, position |
| Read | `portfolio`/`pf`, `balance(s)`/`bal`, `assets`, `borrow`, `lend` | Account, inventory, borrow/lend state |
| Read | `orders`/`ord`/`o`, `position`/`pos`/`p` | Open orders and perp positions |
| Read | `risk`, `config`, `doctor`/`preflight`/`setup check` | Inspect policy and diagnose readiness |
| Markets | `markets`/`market`, `instrument`/`inst`/`instr`/`coin` | Discover/select markets |
| Trade | `buy`/`b`, `sell`/`s`, `scale`, `batch`, `close`, `twap` | Submit entries/exits and exchange TWAPs |
| Manage | `cancel`/`c`, `move`, `resize`, `tp`, `sl`, `trail`/`tsl`, `chase` | Orders and protection |
| Account | `leverage`/`lev`, `margin`, `account mode` | Perp margin/leverage and account mode |
| Shell | `help`/`h`, `clear`/`cls`, `quit`/`exit`/`q` | Help and local terminal controls |

Use `markets all`, `markets perps`, `markets hip3`, or `markets spot`. `config get <key>` inspects
configuration; `risk` and `config` do not mutate it. `borrow`/`lend` are read views. Single-key
bindings (`bind`/`unbind`/`keybinds`/`press`), `@variables` (`set`/`print`/`unset`), and `;`/`sleep`
/ `wait` command chains are intentionally absent; invoke `hl` (or `hl --json`) from external scripts
or agents when sequencing commands.

## Size and price

Sizes: `0.01` base units, `100$` quote notional, `5%` percentage according to command/account
context, or `r100$` / `r0.5%` risk sizing with an attached fixed stop loss (`sl`).
Prices: `40000` absolute, `-100`/`+100` offsets, `-0.5%`/`+0.5%` offsets, or `$entry` when known.
Signed relative levels resolve from a fresh midpoint; bare percentage prices are rejected.
Wire precision and exchange minimums apply after rounding. Risk sizing does not guarantee realized
losses: slippage, gaps, fees, and execution failure can exceed the planned amount.

Spot sells and `close` use available base inventory; fee/lot rounding may leave unsellable dust.
Perp positions support reduce-only, risk sizing, leverage, margin, and position protection; these
features are rejected on spot. `close` is IOC: reduce-only on perps, a normal sell on spot.

## Trading and management

```text
buy 0.01
sell 100$ at 50500 reduce
buy 0.01 at 50000 tif alo post
buy r100$ sl -0.5% tp +1%
scale buy 0.05 into 5 from -0.1% to -1%
scale sell 0.1 reduce into 10 from 42000 to 44000
batch buy 0.01@40000 0.02@39500 tif gtc
batch move 101,102 to 40500
batch resize 101,102 to 0.005
close
close 50% at +0.2%
twap buy 0.1 over 30
twap buy 0.1 over 30 trigger above 50000 max 51000
twap sell 0.1 over 30 trigger below 50000 min 49000
twap cancel all

cancel
cancel 101,102
cancel cloid 0x00000000000000000000000000000001
move 101 to -0.2%
resize 101 to 0.005
tp +1%
sl -0.5%
tp cancel
sl cancel
trail 150
trail set 0.4%
trail 1% size 50% activate 52000
trail cancel
chase buy 0.01 quote
chase sell 100$ 0.2% tif alo post
chase cancel
```

Plain `cancel` skips TP/SL/trailing protection; cancel those explicitly. Eligible ordinary limit
cancellation batches automatically request fast cancel. Mixed batches remain one ordinary action.
Cancel an existing chase before replacing it. Chase cancellation disarms only protection attached
to its own entry; independent protected entries retain their protection intent. Attached protection
starts from a fresh flat position and follows owned entry fills; it requires the daemon to remain
running until the entry finishes. Local validation rejects an invalid batch before submission, but
exchange responses can contain per-item failures: always inspect them.

`trail`/`tsl` places a native reduce-only position trailing stop (`>= 10 USDC` notional after
rounding). Generic move/resize is rejected: cancel and place a new trail explicitly, which resets
its watermark. `sl cancel` targets fixed stops; `trail cancel` targets native trails and disarms
future attached trailing increments. Existing native trails continue while the daemon is offline.

TWAP duration is `5..10080` minutes and minimum notional is `100 USDC` after precision rounding.
Active IDs, conditions, and waiting/running state appear in `status`; cancel with
`twap cancel <id|all>`. Closing a shell does not cancel exchange orders.

## Account controls

```text
leverage cross 5
leverage iso 3
margin add 100
margin remove 25
account mode
account mode require standard
account mode require any
account mode set unified
```

Margin changes require a fresh active isolated perp position. Account-mode changes are account-wide
and require an expiring typed confirmation; an API wallet can move standard mode to unified or
portfolio mode, but transitions requiring the main-user signer reject locally.
