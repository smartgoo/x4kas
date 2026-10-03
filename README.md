# tui4kas - Kaspa Node Monitor

A native desktop app for monitoring Kaspa L1 nodes via wRPC, with an optional embedded node.

Built with [egui/eframe](https://github.com/emilk/egui) and [rusty-kaspa](https://github.com/kaspanet/rusty-kaspa). (The name comes from its origins as a terminal UI.)

## Features

- **Dashboard**: node info, network stats and coin supply, market data (CoinGecko), mining, mempool and fee estimates
- **Mempool**: live transaction table; click a row for details
- **BlockDAG**: interactive DAG visualizer, DAG metrics, GHOSTDAG stats, tip and virtual-parent hashes; click any block for full block info
- **Analytics**: fees, transaction summary, protocol activity, top senders and receivers, each as a table or chart over 1m / 1h / 24h windows
- **RPC Cmds**: run any of 18 RPC methods and inspect formatted responses
- **Node**: configure, start and stop an embedded kaspad, with live status and logs
- **Command palette**: run commands with completion and history

## Prerequisites

- Rust with edition 2024 support
- Access to a Kaspa node, the public resolver, or enough disk space for the embedded node

## Build & Run

```bash
cargo build --release
./target/release/tui4kas
```

### CLI Options

| Flag | Description | Default |
|------|-------------|---------|
| `-u, --url <URL>` | wRPC endpoint (e.g., `ws://127.0.0.1:17110`) | none (see below) |
| `-n, --network <NET>` | Network: `mainnet`, `testnet-10`, `testnet-11` | `mainnet` |
| `-r, --refresh-interval-ms <MS>` | Polling interval in milliseconds | `1000` |

Without `--url`, the app starts the embedded node if `auto_start_daemon` is enabled in its config. Otherwise it waits for you to start the node from the **Node** tab.

### Examples

```bash
# Connect to a local node
tui4kas --url ws://127.0.0.1:17110

# Connect to testnet with 2s refresh
tui4kas --url ws://127.0.0.1:17210 --network testnet-10 --refresh-interval-ms 2000

# Use the embedded node (configure it in the Node tab)
tui4kas
```

### Files

- `~/.tui4kas/config.toml`: embedded node settings (saved from the Node tab)
- `~/.tui4kas/analytics_cache.bin`: analytics cache, saved on exit

## Keyboard Shortcuts

Most actions are also available with the mouse.

| Key | Action |
|-----|--------|
| `1` – `6` | Switch tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `p` | Pause / resume polling |
| `:` or `⌘K` / `Ctrl+K` | Open command palette |
| `?` / `F1` | Toggle help |
| `Esc` | Close popup, palette or help |

In the command palette, `Enter` runs, `Tab` completes and `↑`/`↓` steps through history. Type `help` to list commands.

Closing the window while the embedded node is running stops the node cleanly before exiting.

## Architecture

```
src/
  main.rs               Entry point: CLI, config, tokio runtime, GUI launch
  app.rs                Shared App state (Arc<RwLock<App>>), tabs, command line
  controller.rs         UiCommand handling: node lifecycle, RPC calls, shutdown
  cli.rs                CLI argument parsing (clap)
  config.rs             Embedded node config (~/.tui4kas/config.toml)
  daemon.rs             Embedded kaspad
  daemon_lifecycle.rs   RPC/polling/log-tail startup helpers
  analytics.rs          Chain analytics aggregation
  analytics_streaming.rs Analytics streaming task
  format.rs             Formatting helpers
  rpc/
    client.rs           RpcManager (connect, poll, execute)
    market.rs           CoinGecko market data
    types.rs            UI-friendly RPC type wrappers
  gui/
    mod.rs              GuiApp: frame loop, top bar, shortcuts, quit
    dashboard.rs  mempool.rs  blockdag.rs  analytics.rs  rpc_explorer.rs  node.rs
    command.rs          Command palette
    help.rs             Help window
    theme.rs  widgets.rs
```

The GUI runs on the main thread and never blocks on network I/O. It sends commands to a controller task on a tokio runtime, and background tasks update the shared state and request a repaint.

## License

MIT
