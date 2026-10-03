# tui4kas - Claude Code Instructions

## Build & Check

```bash
cargo build              # compile
cargo fmt                # format (CI runs `cargo fmt --all -- --check`)
cargo clippy --all-targets -- -D warnings  # lint incl. tests (treat warnings as errors)
cargo test               # run test suite (100+ tests)
cargo run -- --url ws://127.0.0.1:17110   # run against a node
```

## Architecture

egui/eframe desktop GUI for monitoring a Kaspa L1 node via wRPC, with an optional embedded kaspad. (The name is historical; the Ratatui TUI was replaced by the GUI.)

### Threading model

- `main` builds a tokio multi-thread `Runtime` by hand (no `#[tokio::main]`). eframe owns the **main thread**, which must stay **outside** the runtime context: `RwLock::blocking_read/blocking_write` panic inside it. `gui::run` enters the runtime only briefly to spawn background tasks, then drops the guard.
- **Shared state:** `Arc<tokio::sync::RwLock<App>>`. The GUI takes `blocking_write` once per frame; background tasks use `.write().await`.
- **Repaint:** background writers call `app.mark_dirty()`, which invokes the injected `App.repaint` hook (`ctx.request_repaint()`). The GUI also schedules a 1s repaint so clocks and uptime tick. Don't busy-repaint.
- **Commands:** the GUI never awaits. It sends `UiCommand`s (`Connect(RemoteTarget)`, `Disconnect`, `StartDaemon`, `StopDaemon`, `ExecuteRpc`, `LookupBlock`, `RunCommandLine`, `Shutdown`) over an mpsc channel to the controller task, which writes results back into `App`.
- **Connections:** exactly one active source at a time (`App.connection: ActiveConnection`): a URL, the public resolver (`RemoteTarget { url: None }`), or the embedded node. Switching calls `Controller::stop_all` (abort polling, stop the daemon, save the analytics cache, `App::clear_node_data`). Stopping the embedded node falls back to the remembered remote target. Mining and analytics only run with a direct node (`has_direct_node`: URL or embedded), not the resolver.
- **Task tracking:** every background task (node polling via `create_and_start_rpc`, mining, analytics) is stored in `PollingHandles` so `abort_all` stops it. Never spawn an untracked task that writes node data, or it keeps writing stale data after a switch.
- **Graceful quit:** a close request while a node is running is cancelled, `Shutdown(oneshot)` is sent, a "Stopping node…" modal is shown, and the window closes once the controller signals completion (daemon stopped, analytics cache saved).

### Module Layout

- `src/main.rs`: entry point. Parses CLI, loads `DaemonConfig`, builds the runtime, calls `gui::run`.
- `src/app.rs`: central state. `App`, `Tab` (6 tabs), `CommandLine`, `RpcExplorerState`, `DagVisualizer`, `DagSelection`, analytics state (`TimeWindow`, `ViewMode`), and `mark_dirty()`.
- `src/cli.rs`: clap args (`--url`, `--network`, `--refresh-interval-ms`).
- `src/controller.rs`: the `UiCommand` enum, `RemoteTarget`, and the controller task. Owns the RPC manager, daemon handle, polling and log-tail handles. Handles the startup modes (`--url`, auto-start daemon, or idle with the connection window open), connect/disconnect, daemon start/stop, command-line execution, and shutdown.
- `src/daemon.rs`: embedded kaspad (`DaemonHandle`).
- `src/daemon_lifecycle.rs`: RPC creation, mining polling, log tailing, start-daemon-and-connect helpers.
- `src/config.rs`: `DaemonConfig` (`~/.tui4kas/config.toml`) and `ConnectionSettings`/`ConnectionKind`, the last connection choice (`~/.tui4kas/connection.toml`).
- `src/analytics.rs` / `src/analytics_streaming.rs`: chain analytics aggregation and its streaming task (cache at `~/.tui4kas/analytics_cache.bin`).
- `src/format.rs`: pure formatting helpers (`format_hashrate`, `format_usd`, `truncate_hash`, …).
- `src/rpc/client.rs`: `RpcManager`. Connect, background polling, RPC execution, mining/analytics fetches, block lookup.
- `src/rpc/market.rs`: CoinGecko market polling (every 60s).
- `src/rpc/types.rs`: UI-friendly structs with `From` impls for kaspa RPC types.
- `src/gui/mod.rs`: `GuiApp` (`eframe::App`). Frame loop, top bar (status, network, pause, tabs, help/palette buttons), keyboard shortcuts, quit handling.
- `src/gui/dashboard.rs`: Dashboard tab (node info, network + supply, markets, mining, mempool & fees cards).
- `src/gui/mempool.rs`: Mempool tab (`egui_extras` table, click a row for the detail window).
- `src/gui/blockdag.rs`: BlockDAG tab (custom painter DAG visualizer, metrics, GHOSTDAG stats, tip/parent lists, Block Info window).
- `src/gui/analytics.rs`: Analytics tab (5 panels with a Table/Chart toggle and time window; `egui_plot` charts).
- `src/gui/rpc_explorer.rs`: RPC Cmds tab (method list and read-only result viewer).
- `src/gui/node.rs`: Node tab (settings form bound to a `DaemonConfig` copy, Start/Stop/Save, status, log viewer).
- `src/gui/connection.rs`: `ConnectionWindow`, opened from the top-bar status button. Custom URL / public resolver / embedded node, network, Connect/Disconnect.
- `src/gui/command.rs`: command palette (bottom panel: input, suggestions, output).
- `src/gui/help.rs`: help window (shortcuts).
- `src/gui/theme.rs`: color constants and status → label/color mapping.
- `src/gui/widgets.rs`: shared building blocks (`card`, `kv_grid`/`kv`, `placeholder`, `syncing_guard`).

### Dependencies

- `eframe` / `egui_extras` 0.33, `egui_plot` 0.34. Upgrade them in lockstep; import egui as `eframe::egui`.
- Kaspa crates (`kaspa-rpc-core`, `kaspa-wrpc-client`, `kaspad`, …) pinned to git rev `01b532e` (rusty-kaspa v2.1.0). Bump all of them together.
- `reqwest` for CoinGecko.
- Rust edition 2024. Let-chains are fine, and clippy prefers them over nested `if let`.

## Conventions

- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` after changes. Tests cover app state, config, types, formatting and analytics. GUI code is not unit-tested.
- Keep async and RPC work out of `gui/`. Add a `UiCommand` and handle it in the controller instead.
- Background code that mutates `App` must call `app.mark_dirty()` so the GUI repaints.
- GUI-only view state (popups open, help visible) lives in `GuiApp` or egui memory, not `App`, unless tests need it.
- All RPC types have UI-friendly wrapper structs in `rpc/types.rs`. Don't use raw kaspa types in UI code.
- Use `theme::*` colors and `widgets::*` helpers for a consistent look. Labels use `.weak()`.
- Shortcuts are ignored while a text field has focus (`ctx.wants_keyboard_input()`), except Cmd/Ctrl+K.
- Shortcuts: `1`–`6` tabs, Ctrl+Tab / Ctrl+Shift+Tab cycle, `p` pause, `:` or Cmd/Ctrl+K palette, `?`/F1 help, Esc closes popups.
- 18 RPC methods are available in both the RPC Cmds tab and the command palette. `RpcManager` has a test ensuring every method has a handler.
