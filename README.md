# x4kas - Kaspa Node Monitor

A native desktop app for monitoring Kaspa L1 nodes via wRPC, connecting to a node by URL or through the public resolver, plus a command-line tool (`x4kas-cli`) for scripts and AI agents.

Built with [egui/eframe](https://github.com/emilk/egui) and [rusty-kaspa](https://github.com/kaspanet/rusty-kaspa). (The name comes from its origins as a terminal UI.)

> **Status:** early development. Expect breaking changes and rough edges. This is an unofficial community project, not affiliated with the Kaspa core developers.

## Features

- **Dashboard**: one responsive page whose cards reflow into rows as the window resizes
  - Live, interactive BlockDAG visualizer; click any block for full block info
  - BlockDAG stats (DAA and blue score, sink, pruning point, tips, DAG width, block interval, blue block rate) and market data (CoinGecko)
  - Supply (incl. unspendable burn-address balance, block reward and next reduction), mining (difficulty, hashrate, unique miners) and node info
  - Chain analytics (direct node only): transactions per 10 minutes over 24h, transaction summary, mempool, transaction inspection (opcodes, covenants, protocols), fees, mining share by node version, top miners, top senders and receivers, each over a 1m / 1h / 24h window
- **Mempool**: live transaction table; click a row for details
- **RPC Cmds**: run any of 36 read-only RPC methods (with argument forms for those that take a hash, address or number) and inspect formatted responses
- **Connection switcher**: connect to a node by URL or through the public resolver, from inside the app
- **Integrated terminal**: your login shell in a bottom pane (`` Ctrl+` ``), a full PTY terminal

## Prerequisites

- Access to a Kaspa node or the public resolver

## Install

Build from source (Rust 1.91+; on Linux also OpenSSL headers, e.g. `libssl-dev` and `pkg-config`):

Two binaries are built: `x4kas` (the desktop GUI) and `x4kas-cli` (headless; it doesn't link any GUI libraries, so it also runs on servers without a display).

```bash
cargo install --git https://github.com/smartgoo/x4kas x4kas-gui x4kas-cli
# or, from a clone
cargo install --path crates/x4kas-gui    # installs `x4kas`
cargo install --path crates/x4kas-cli    # installs `x4kas-cli`
# or just build both into target/release/
cargo build --release
```

## Usage

### GUI Options

| Flag | Description | Default |
|------|-------------|---------|
| `-u, --url <URL>` | Connect to this wRPC endpoint on startup (e.g., `ws://127.0.0.1:17110`) | none (see below) |
| `-n, --network <NET>` | Network: `mainnet`, `testnet-10`, `testnet-11` | `mainnet` |
| `-r, --refresh-interval-ms <MS>` | Polling interval in milliseconds | `1000` |

Without `--url`, the app opens the **Connection** window, where you choose:

- **Custom URL**: any node's Borsh wRPC endpoint (port 17110 on mainnet, 17210 on testnets)
- **Public resolver**: a public node chosen by the Kaspa resolver (chain analytics needs a direct node, so it's disabled)

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

### CLI

`x4kas-cli` runs one command, prints the result to stdout and exits, with a nonzero exit code on error. Without `--url` it goes through the public resolver.

| Flag | Description | Default |
|------|-------------|---------|
| `-u, --url <URL>` | wRPC endpoint URL | public resolver |
| `-n, --network <NET>` | Network: `mainnet`, `testnet-10`, `testnet-11` | `mainnet` |
| `-t, --timeout <SECS>` | Connect timeout | `20` |

Flags can go before or after the command.

#### RPC

Every method in the GUI's **RPC Cmds** tab is available as `x4kas-cli rpc <method>`, and prints the JSON response in full (the GUI cuts responses at 1 MB).

```bash
x4kas-cli rpc --help                      # list all methods
x4kas-cli rpc get_block --help            # a method's arguments and defaults
x4kas-cli rpc get_block_dag_info --url ws://127.0.0.1:17110
x4kas-cli rpc get_block <hash> false      # optional arguments are positional
x4kas-cli rpc get_balances_by_addresses kaspa:qa… kaspa:qb…   # lists: separate words or commas
x4kas-cli rpc get_sink -n testnet-10 -u ws://127.0.0.1:17210 -t 5
```

### Network Access

Besides the node you connect to, x4kas contacts:

- the CoinGecko API every 60s for market data ([data provided by CoinGecko](https://www.coingecko.com/en/api))
- the public Kaspa resolver, only when you choose it (or run `x4kas-cli` without `--url`)

### Files

- `~/.x4kas/connection.toml`: last connection choice (URL, network, mode)
- `~/.x4kas/analytics_cache.bin`: analytics cache, saved on exit

## Keyboard Shortcuts

Most actions are also available with the mouse.

| Key | Action |
|-----|--------|
| `1` – `3` | Switch tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `p` | Pause / resume polling |
| `` Ctrl+` `` | Show / hide the terminal |
| `?` / `F1` | Toggle help |
| `Esc` | Close popup or help |

Click into the terminal to type; app shortcuts are off until you click elsewhere. The shell keeps running while the pane is hidden, and `exit` closes it.

## Architecture

```
crates/
  x4kas-core/src/       Shared library (no GUI): everything below the frontends
    app.rs                Shared App state (Arc<RwLock<App>>), tabs (Dashboard, Mempool, RPC Cmds)
    controller.rs         UiCommand handling: connections, RPC calls, shutdown
    config.rs             Saved connection choice (~/.x4kas/connection.toml)
    polling.rs            RPC creation and background polling tasks
    analytics.rs          Chain analytics aggregation
    analytics_streaming.rs Analytics streaming task
    emission.rs           Block reward schedule and burn address
    format.rs             Formatting helpers
    tx_inspect.rs         Per-transaction classification (scripts, opcodes, protocols)
    rpc/
      client.rs           RpcManager (connect, poll, execute)
      market.rs           CoinGecko market data
      methods.rs          RPC method catalog and argument parsing
      hash_links.rs       Finds block hashes in RPC responses for linking
      types.rs            UI-friendly RPC type wrappers
  x4kas-gui/src/        `x4kas` binary
    main.rs               Entry point: args, tokio runtime, GUI launch
    gui/
      mod.rs              GuiApp: frame loop, top bar, shortcuts, quit
      dashboard.rs        Dashboard tab: card layout and node-backed cards
      blockdag.rs         DAG visualizer, BlockDAG card, Block Info window
      analytics.rs        Dashboard's chain analytics cards and tx chart
      mempool.rs          Mempool tab
      rpc_explorer.rs     RPC Cmds tab
      connection.rs       Connection window (URL / resolver)
      terminal.rs         Integrated terminal pane
      help.rs             Help window
      theme.rs  widgets.rs
  x4kas-cli/src/        `x4kas-cli` binary
    main.rs               Entry point: global args, command dispatch
    rpc.rs                `rpc <method>` subcommands generated from the method catalog
vendor/egui_term/       Terminal widget (alacritty_terminal), vendored with small patches
```

The GUI and CLI are thin frontends over `x4kas-core`. The GUI runs on the main thread and never blocks on network I/O. It sends commands to a controller task on a tokio runtime, and background tasks update the shared state and request a repaint.

## Contributing

Issues and pull requests are welcome. Before opening a PR, run:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT, see [LICENSE](LICENSE). Built binaries also include third-party dependencies under their own licenses, including the LGPL-3.0 `malachite` crates used by rusty-kaspa.
