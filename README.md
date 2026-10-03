# x4kas - Kaspa Node Monitor

A native desktop app for monitoring Kaspa L1 nodes via wRPC, connecting to a node by URL or through the public resolver.

Built with [egui/eframe](https://github.com/emilk/egui) and [rusty-kaspa](https://github.com/kaspanet/rusty-kaspa). (The name comes from its origins as a terminal UI.)

> **Status:** early development. Expect breaking changes and rough edges. This is an unofficial community project, not affiliated with the Kaspa core developers.

## Features

- **Dashboard**: node info, network stats and coin supply, market data (CoinGecko), mempool and fee estimates
- **Mempool**: live transaction table; click a row for details
- **BlockDAG**: interactive DAG visualizer, DAG metrics, GHOSTDAG stats, tip and virtual-parent hashes; click any block for full block info
- **Analytics** (direct node only): transaction summary, fees, transaction inspection (opcodes, covenants, protocols), mining share by node version, mining analysis, top senders and receivers, each over a 1m / 1h / 24h window
- **RPC Cmds**: run any of 36 read-only RPC methods (with argument forms for those that take a hash, address or number) and inspect formatted responses
- **Connection switcher**: connect to a node by URL or through the public resolver, from inside the app
- **Integrated terminal**: your login shell in a bottom pane (`` Ctrl+` ``), a full PTY terminal

## Prerequisites

- Access to a Kaspa node or the public resolver

## Install

Build from source (Rust 1.91+; on Linux also OpenSSL headers, e.g. `libssl-dev` and `pkg-config`):

```bash
cargo install --git https://github.com/smartgoo/x4kas
# or, from a clone
cargo build --release
./target/release/x4kas
```

## Usage

### CLI Options

| Flag | Description | Default |
|------|-------------|---------|
| `-u, --url <URL>` | Connect to this wRPC endpoint on startup (e.g., `ws://127.0.0.1:17110`) | none (see below) |
| `-n, --network <NET>` | Network: `mainnet`, `testnet-10`, `testnet-11` | `mainnet` |
| `-r, --refresh-interval-ms <MS>` | Polling interval in milliseconds | `1000` |

Without `--url`, the app opens the **Connection** window, where you choose:

- **Custom URL**: any node's Borsh wRPC endpoint (port 17110 on mainnet, 17210 on testnets)
- **Public resolver**: a public node chosen by the Kaspa resolver (mining and analytics need a direct node, so they're disabled)

Click the connection button in the bottom status bar to open the window again at any time. Switching connections stops whatever was running. Your last choice is saved and pre-filled next time.

### Examples

```bash
# Connect to a local node
x4kas --url ws://127.0.0.1:17110

# Connect to testnet with 2s refresh
x4kas --url ws://127.0.0.1:17210 --network testnet-10 --refresh-interval-ms 2000

# Pick a connection in the app (URL or public resolver)
x4kas
```

### RPC Commands

Every method in the **RPC Cmds** tab can also be run from the command line. It connects, prints the JSON response to stdout and exits, with a nonzero exit code on error. Without `--url` it goes through the public resolver.

```bash
x4kas rpc --help                      # list all methods
x4kas rpc get_block --help            # a method's arguments and defaults
x4kas rpc get_block_dag_info --url ws://127.0.0.1:17110
x4kas rpc get_block <hash> false      # optional arguments are positional
x4kas rpc get_balances_by_addresses kaspa:qa… kaspa:qb…   # lists: separate words or commas
x4kas rpc get_sink -n testnet-10 -u ws://127.0.0.1:17210 -t 5   # -t: connect timeout (s), default 20
```

Responses are never truncated (the GUI cuts them at 1 MB).

### Network Access

Besides the node you connect to, x4kas contacts:

- the CoinGecko API every 60s for market data ([data provided by CoinGecko](https://www.coingecko.com/en/api))
- the public Kaspa resolver, only when you choose it (or run `x4kas rpc` without `--url`)

### Files

- `~/.x4kas/connection.toml`: last connection choice (URL, network, mode)
- `~/.x4kas/analytics_cache.bin`: analytics cache, saved on exit

## Keyboard Shortcuts

Most actions are also available with the mouse.

| Key | Action |
|-----|--------|
| `1` – `5` | Switch tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `p` | Pause / resume polling |
| `` Ctrl+` `` | Show / hide the terminal |
| `?` / `F1` | Toggle help |
| `Esc` | Close popup or help |

Click into the terminal to type; app shortcuts are off until you click elsewhere. The shell keeps running while the pane is hidden, and `exit` closes it.

## Architecture

```
src/
  main.rs               Entry point: CLI, tokio runtime, GUI launch
  app.rs                Shared App state (Arc<RwLock<App>>), tabs
  controller.rs         UiCommand handling: connections, RPC calls, shutdown
  cli.rs                CLI argument parsing (clap)
  config.rs             Saved connection choice (~/.x4kas/connection.toml)
  polling.rs            RPC creation and background polling tasks
  analytics.rs          Chain analytics aggregation
  analytics_streaming.rs Analytics streaming task
  format.rs             Formatting helpers
  tx_inspect.rs         Per-transaction classification (scripts, opcodes, protocols)
  rpc/
    client.rs           RpcManager (connect, poll, execute)
    market.rs           CoinGecko market data
    methods.rs          RPC method catalog and argument parsing
    hash_links.rs       Finds block hashes in RPC responses for linking
    types.rs            UI-friendly RPC type wrappers
  gui/
    mod.rs              GuiApp: frame loop, top bar, shortcuts, quit
    dashboard.rs  mempool.rs  blockdag.rs  analytics.rs  rpc_explorer.rs
    connection.rs       Connection window (URL / resolver)
    terminal.rs         Integrated terminal pane
    help.rs             Help window
    theme.rs  widgets.rs
vendor/egui_term/       Terminal widget (alacritty_terminal), vendored with small patches
```

The GUI runs on the main thread and never blocks on network I/O. It sends commands to a controller task on a tokio runtime, and background tasks update the shared state and request a repaint.

## Contributing

Issues and pull requests are welcome. Before opening a PR, run:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT, see [LICENSE](LICENSE). Built binaries also include third-party dependencies under their own licenses, including the LGPL-3.0 `malachite` crates used by rusty-kaspa.
