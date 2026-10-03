# x4kas - Claude Code Instructions

## Build & Check

```bash
cargo build              # compile the workspace (both binaries)
cargo fmt --all          # format (CI runs `cargo fmt --all -- --check`)
cargo clippy --all-targets -- -D warnings  # lint incl. tests (treat warnings as errors)
cargo test               # run test suite (100+ tests, all crates)
cargo run --bin x4kas -- --url ws://127.0.0.1:17110   # GUI against a node
cargo run --bin x4kas-cli -- rpc get_info             # CLI (public resolver without --url)
```

## Architecture

Monitors a Kaspa L1 node via wRPC, connecting by URL or through the public resolver. A Cargo workspace with two binaries over one shared library:

- `crates/x4kas-core` (lib `x4kas_core`): everything below the frontends: RPC, app state, controller, polling, analytics, config, formatting. No egui and no clap.
- `crates/x4kas-gui` (package `x4kas-gui`, binary `x4kas`): the egui/eframe desktop GUI.
- `crates/x4kas-cli` (binary `x4kas-cli`): headless commands that connect, print and exit. It must not depend on egui/eframe, so it builds fast and runs on servers without a display. The CLI will grow to cover most of what the GUI shows: put the data logic in core and keep only argument parsing and printing in the CLI crate.

### Threading model

- (GUI) `main` builds a tokio multi-thread `Runtime` by hand (no `#[tokio::main]`). eframe owns the **main thread**, which must stay **outside** the runtime context: `RwLock::blocking_read/blocking_write` panic inside it. `gui::run` enters the runtime only briefly to spawn background tasks, then drops the guard.
- **Shared state:** `Arc<tokio::sync::RwLock<App>>`. The GUI takes `blocking_write` once per frame; background tasks use `.write().await`.
- **Repaint:** background writers call `app.mark_dirty()`, which invokes the injected `App.repaint` hook (`ctx.request_repaint()`). The GUI also schedules a 1s repaint so clocks and uptime tick. Don't busy-repaint.
- **Commands:** the GUI never awaits. It sends `UiCommand`s (`Connect(RemoteTarget)`, `Disconnect`, `ExecuteRpc`, `LookupBlock`, `Shutdown`) over an mpsc channel to the controller task, which writes results back into `App`.
- **Connections:** exactly one active source at a time (`App.connection: ActiveConnection`): a URL or the public resolver (`RemoteTarget { url: None }`). Switching calls `Controller::stop_all` (abort polling, disconnect, save the analytics cache, `App::clear_node_data`). Analytics (including mining stats) only runs with a direct node (`ActiveConnection::is_direct`: a URL), not the resolver; hashrate polling runs with either. There is no embedded node for now.
- **Task tracking:** every background task (node polling via `create_and_start_rpc`, hashrate, analytics, and one-off frontend requests via `PollingHandles::spawn_request` / `Controller::spawn_rpc`) is stored in `PollingHandles` so `abort_all` stops it. Never spawn an untracked task that writes `App`, or it keeps writing stale data after a switch.
- **Graceful quit:** a close request is cancelled, `Shutdown(oneshot)` is sent, a "Shutting down…" modal is shown, and the window closes once the controller signals completion (disconnected, analytics cache saved).

### Module Layout

Paths below are relative to each crate's `src/`.

**x4kas-gui**

- `main.rs`: entry point. Parses `Args` (`--url`, `--network`, `--refresh-interval-ms`), builds the runtime, calls `gui::run`.
- `gui/mod.rs`: `GuiApp` (`eframe::App`). Frame loop, top bar (brand, tab strip, terminal/help buttons), bottom status bar (connection, node sync and analytics indicators with details on hover, network, DAA score colored by lag behind the DAG tip, pause), keyboard shortcuts, quit handling.
- `gui/dashboard.rs`: Dashboard tab (node info + block counts, markets, network stats + hashrate + supply, mempool & fees cards).
- `gui/mempool.rs`: Mempool tab (`egui_extras` table, click a row for the detail window).
- `gui/blockdag.rs`: BlockDAG tab (DAG visualizer modeled on the Kaspalytics home page: newest DAA scores as columns, parent edges, tips highlighted, hovering pauses it, then hover/click blocks; animation state in egui memory, metrics, GHOSTDAG stats, tip/parent lists, Block Info window).
- `gui/analytics.rs`: Analytics tab, modeled on the Kaspalytics home page. Rows: Transaction Summary (tx count, TPS, output script classes) / Fees (node fee-rate estimate, average and total accepted fees per window), Transaction Inspection (opcodes, covenants, protocols), Mining Share by Node Version / Mining Analysis (hashrate, unique and top miners), Top Senders / Top Receivers. Panels have a time window dropdown in the card header (`panel_card` hands the contents that window's `AggregatedView`; windows in `AnalyticsState::windows`, indexed by `AnalyticsPanel`).
- `gui/rpc_explorer.rs`: RPC Cmds tab (method list, argument form, Loop toggle, read-only JSON result viewer (`widgets::json_view`) with 🔍 links on block hashes that run `get_block`).
- `gui/connection.rs`: `ConnectionWindow`, opened from the status-bar connection button. Custom URL / public resolver, network, Connect/Disconnect.
- `gui/terminal.rs`: `TerminalPane`, the integrated terminal (bottom panel, toggled with Ctrl+` or the `>_` button). Runs the user's login shell (`$SHELL -l`, home dir) on a real PTY via `egui_term`; started on first open, kept alive while hidden, closed on shell exit. Focus follows clicks; while focused, app shortcuts and Esc go to the shell. Colors from `theme::terminal_palette`.
- `gui/help.rs`: help window (shortcuts).
- `gui/theme.rs`: palette constants, `apply` (monospace fonts + dark visuals, installed at startup), and status → label/color mapping.
- `gui/widgets.rs`: shared building blocks (`card` with the title set into its border and `card_with_header` for widgets after the title, `kv_grid`/`kv`, `section_title` (also table headers), `modal_window` (centered, closes on X/Esc), `or_dash`, `json_view` (JSON with block-hash links), `primary_button`, `status_chip`, `placeholder`, `direct_node_placeholder`, `CARD_GAP`, `fit_label`, and `address`/`block_hash`: fitted value, copy icon and hover highlight; a click on an address opens an explorer menu, on a block hash the Block Info window; `copy_value` for plain text with a copy icon).

**x4kas-core**

- `app.rs`: central state, free of GUI types. `App`, `Tab` (5 tabs), `RpcExplorerState`, `DagVisualizer`, `DagSelection`, analytics state (`TimeWindow`, `AnalyticsPanel` windows), and `mark_dirty()`.
- `controller.rs`: the `UiCommand` enum, `RemoteTarget`, and the controller task. Owns the RPC manager and polling handles. Handles startup (`--url`, or idle with the connection window open), connect/disconnect, RPC execution, and shutdown.
- `polling.rs`: `PollingHandles` (task handles plus a `JoinSet` of one-off requests), RPC creation (`create_and_start_rpc`) and hashrate polling.
- `config.rs`: `data_dir()` (`~/.x4kas`), `valid_networks()`, and `ConnectionSettings`/`ConnectionKind`, the last connection choice (`~/.x4kas/connection.toml`).
- `analytics.rs` / `analytics_streaming.rs`: chain analytics (`summarize_chain_blocks` turns VSPC v2 responses into per-chain-block `Metrics`, incl. miners from coinbases; rolled into 1m/10m buckets; `AggregatedView` per window) and its VSPC v2 streaming task (cache at `~/.x4kas/analytics_cache.bin`, versioned by `CACHE_MAGIC`; bump it when the format changes). The task reports its progress in `app.analytics.status` (`AnalyticsStatus`/`AnalyticsPhase`), waits for a connected, synced node, and retries failed requests instead of exiting. All data comes from the connected node.
- `tx_inspect.rs`: per-transaction classification, mirroring Kaspalytics: protocol detection, output script classes, covenant/introspection/ZK opcode scanning, coinbase node-version parsing.
- `format.rs`: pure formatting helpers shared by GUI and CLI (`format_number`, `format_kas`, `sompi_to_kas`, `format_hashrate`, `format_usd`, `format_duration`, explorer URLs, `now_ms`). Show KAS amounts with `format_kas`.
- `rpc/client.rs`: `RpcManager`. Connect (`connect` retries forever for the GUI; `connect_once` fails fast for the CLI), background polling, the `BlockAdded` stream that feeds the DAG visualizer (`stream_blocks`, joined with polling in the `handles.node` task, resubscribing on every connect), RPC execution (`rpc_json` returns the full response; `execute_rpc_call` truncates it for the GUI and is also used for Block Info's `get_block`), hashrate estimate, VSPC v2 fetches.
- `rpc/market.rs`: CoinGecko market polling (every 60s).
- `rpc/methods.rs`: `RPC_METHODS` catalog (name, description, typed params with defaults), `resolve_args`, and argument parsers.
- `rpc/hash_links.rs`: finds block hashes (by field name) in JSON responses so the result viewer can link them to `get_block`.
- `rpc/types.rs`: UI-friendly structs with `From` impls for kaspa RPC types.

**x4kas-cli**

- `main.rs`: `#[tokio::main]` entry point. Global clap args (`--url`, `--network`, `--timeout`, usable before or after the subcommand) and the `Command` enum, one variant per command group.
- `rpc.rs`: `x4kas-cli rpc <method> [args…]`. `RpcCall` implements clap's `Subcommand` by hand, generating one subcommand per `RPC_METHODS` entry (positional args; a trailing list takes several words), so the CLI and RPC Cmds tab never drift. `run` validates args (`RpcMethod::validate_args`), connects (`RpcManager::connect_once`; resolver attempts retry until `--timeout`, since it sometimes hands out a dead node), and prints `rpc_json` untruncated.

### Dependencies

- `eframe` / `egui_extras` 0.33. Upgrade them in lockstep; import egui as `eframe::egui`.
- Kaspa crates (`kaspa-rpc-core`, `kaspa-wrpc-client`) pinned to git rev `01b532e` (rusty-kaspa v2.1.0). Bump all of them together.
- `reqwest` for CoinGecko.
- `egui_term` (terminal widget on `alacritty_terminal`) is vendored in `vendor/egui_term` (excluded from the workspace, so not formatted or linted) from upstream rev `9a631ca`, the last egui 0.33 revision. Local patches are marked `x4kas:`: keyboard input needs focus only (upstream also required the pointer over the widget), Tab/arrows/Esc are focus-locked to the shell, and a closed event channel no longer panics. Re-vendor when upgrading egui.
- Rust edition 2024. Let-chains are fine, and clippy prefers them over nested `if let`.

## Conventions

- Run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test` after changes. Tests cover app state, config, types, formatting, analytics and CLI parsing. GUI code is not unit-tested.
- Shared dependency versions live in the root `[workspace.dependencies]`; crates use `dep.workspace = true`.
- Keep async and RPC work out of `gui/`. Add a `UiCommand` and handle it in the controller instead.
- GUI and CLI share core code. Don't put data fetching or computation in either frontend crate.
- Background code that mutates `App` must call `app.mark_dirty()` so the GUI repaints.
- GUI-only view state (popups open, help visible) lives in `GuiApp` or egui memory, not `App`, unless tests need it.
- All RPC types have UI-friendly wrapper structs in `rpc/types.rs`. Don't use raw kaspa types in UI code.
- Show every Kaspa address with `widgets::address` and every block hash with `widgets::block_hash`, never a plain label. A block hash click calls `widgets::request_block`; the frame loop in `gui/mod.rs` sends `LookupBlock` and draws the Block Info window (`blockdag::block_window`: hash, explorer links, and the `get_block` JSON whose block hashes link onward) on any tab.
- Use `theme::*` colors and `widgets::*` helpers for a consistent look; never hard-code `Color32`s in views. Labels use `.weak()`. All text is already monospace, so don't add `.monospace()`.
- Shortcuts are ignored while a text field or the terminal has focus (`ctx.wants_keyboard_input()`, `TerminalPane::has_focus`), except Ctrl+`.
- Shortcuts: `1`–`5` tabs, Ctrl+Tab / Ctrl+Shift+Tab cycle, `p` pause, Ctrl+` terminal, `?`/F1 help, Esc closes popups.
- 36 read-only RPC methods (listed in `rpc/methods.rs`) are available in the RPC Cmds tab and as `x4kas-cli rpc <method>` (lists are comma-separated). Responses are pretty-printed JSON (`to_json`; wrap bare values in a `json!` object). To add one, add it to `RPC_METHODS` and a match arm in `RpcManager::rpc_json`; a test checks every method has an arm. It then appears in the CLI (`x4kas-cli rpc <method>`) automatically. State-changing calls (`submit_*`, `add_peer`, `ban`/`unban`, `resolve_finality_conflict`, `shutdown`) are intentionally not exposed.
