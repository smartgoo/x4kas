# x4kas - Kaspa Terminal

A Bloomberg Terminal inspired all-in-one Kaspa desktop app.

An amalgamation of (probably far) too much in one monorepo.

A native desktop app for monitoring Kaspa L1 nodes via wRPC, connecting to a node by URL or through the public resolver, plus a command-line tool (`x4kas-cli`) for scripts and AI agents.

Built with [egui/eframe](https://github.com/emilk/egui) and [rusty-kaspa](https://github.com/kaspanet/rusty-kaspa). (The name comes from its origins as a terminal UI.)

> **Status:** early development. Expect breaking changes and rough edges. This is an unofficial community project, not affiliated with the Kaspa core developers.

## Features

- **Dashboard**: one responsive page whose cards reflow into rows as the window resizes
  - Live, interactive BlockDAG visualizer; click any block for full block info
  - BlockDAG stats (DAA and blue score, sink, pruning point, tips, DAG width, block interval, blue block rate) and market data (CoinGecko)
  - Supply (incl. unspendable burn-address balance, block reward and next reduction), mining (difficulty, hashrate, unique miners) and node info
  - Chain analytics (direct node only): transactions per 10 minutes over 24h, transaction summary, mempool, transaction inspection (opcodes, covenants, protocols), fees, mining share by node version, top miners, top senders and receivers, each over a 1m / 1h / 24h window
- **Monitoring**: a watchlist with live balances, pending mempool activity, 24h change, confirmed events and rule-based alerts (any activity, received/sent thresholds, balance crossings, first activity after N hours idle), fed by the node's `UtxosChanged` notifications; alerts pop up as toasts on any tab and count on the Monitoring tab until seen; works through the resolver too when the node has `--utxoindex`
  - **Address Info** from any address in the app: label, balance, balance history, transactions, counterparties, the likely-owner cluster and watch settings, backed by a local address index of the node's retention window (direct node only; see below)
  - **Clustering**: addresses that spend together, and probable change outputs, are grouped into likely owners (with guards against merging labelled entities, L2 bridges and covenant contracts, and a size cap), so an exchange's many deposit addresses read as one
  - **Flow graph**: follow the money hop by hop from any address; click a node to expand it, fold pass-through chains (peel chains) into one hop-counted edge, export the graph as CSV or JSON
  - **Peel chains**: Address Info shows which link of a chain of single-input, two-output spends an address is, with what each spend peeled off
  - **Export**: an address's indexed transactions and any flow graph as CSV or JSON, under `~/.x4kas/exports/`
  - **Labels** from the public api.kaspa.org list (exchanges, pools, funds, bridges; fetched on launch and hourly, nothing bundled) and your own, shown as chips wherever an address appears; opt-in per-address lookups on kas.fyi (API key, with the entity's link and categories) and KNS `.kas` names; pools that mine many blocks are labelled from their coinbase tags as the chain streams in
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

#### Addresses, index, watch and labels

The GUI builds a local **address index** of everything the node still serves (its retention window, ~30 hours on mainnet; `kaspad --retention-period-days` keeps more) from the VSPC v2 stream it already reads for analytics, under `~/.x4kas/index/<network>/`. Six-hour slabs are dropped as the node prunes, so disk use is bounded by the window. `x4kas-cli index run` builds the same index headlessly on a server; the `address` commands read it without a node. The store has a single-writer lock, so a CLI query while the GUI (or `index run`) is writing it reports that the index is in use.

```bash
x4kas-cli index run --url ws://127.0.0.1:17110 --backfill-hours 0   # keep indexing until Ctrl+C (0 = everything the node has)
x4kas-cli index status
x4kas-cli address profile kaspa:qq…        # totals over the indexed window, cluster size, peel chain if any
x4kas-cli address txs kaspa:qq… --limit 50 # newest first; pass `next` back as --before for the next page
x4kas-cli address txs kaspa:qq… --all --format csv > txs.csv   # every indexed transaction as CSV
x4kas-cli address peers kaspa:qq… --top 20 # counterparties by volume
x4kas-cli address cluster kaspa:qq…        # likely-owner cluster and a sample of members
x4kas-cli address flows kaspa:qq… --hops 2 # follow the money as a graph (nodes and edges)
x4kas-cli address flows kaspa:qq… --collapse --format csv       # pass-through chains folded, one row per edge
x4kas-cli address balance kaspa:qq… --now <sompi>   # balance history from the index's deltas
x4kas-cli address tx <txid>
x4kas-cli watch kaspa:qq… kaspa:qz… --balances      # one JSON line per confirmed change and alert (needs --utxoindex on the node)
x4kas-cli watch                              # the saved watchlist
x4kas-cli labels get bybit                   # by address or by name
x4kas-cli labels set kaspa:qq… "My cold wallet"
x4kas-cli labels refresh                     # fetch the api.kaspa.org list now
x4kas-cli labels key <kas.fyi api key>       # enable kas.fyi tag lookups (per address, opt-in)
x4kas-cli labels kns on                      # enable .kas name resolution (per address, opt-in)
x4kas-cli labels online kaspa:qq…            # ask the enabled online sources
```

### Network Access

Besides the node you connect to, x4kas contacts:

- the CoinGecko API every 60s for market data ([data provided by CoinGecko](https://www.coingecko.com/en/api))
- `api.kaspa.org/addresses/names` on launch and hourly for the public address labels (the whole list, so nothing about which addresses you look at leaves your machine)
- `api.kas.fyi` and `api.knsdomains.org`, only if you enable them (`x4kas-cli labels key` / `labels kns on`) and only for the address you press "Look up" on; answers are cached for a week
- the public Kaspa resolver, only when you choose it (or run `x4kas-cli` without `--url`)

### Files

- `~/.x4kas/connection.toml`: last connection choice (URL, network, mode)
- `~/.x4kas/analytics_cache.bin`: analytics cache, saved on exit
- `~/.x4kas/index/<network>/`: the address index (dropped and rebuilt when its format changes)
- `~/.x4kas/watchlist.toml`: watched addresses, names and alert rules
- `~/.x4kas/labels/user.toml`: your address labels; `labels/kaspa_org.json`: the cached public list; `labels/settings.toml` and `labels/online_cache.json`: online lookup settings and answers

## Keyboard Shortcuts

Most actions are also available with the mouse.

| Key | Action |
|-----|--------|
| `1` – `4` | Switch tab |
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
    app.rs                Shared App state (Arc<RwLock<App>>), tabs (Dashboard, Monitoring, Mempool, RPC Cmds)
    controller.rs         UiCommand handling: connections, RPC calls, address lookups, watchlist, labels, shutdown
    config.rs             Saved connection choice (~/.x4kas/connection.toml)
    polling.rs            RPC creation and background polling tasks
    chain_stream.rs       The VSPC v2 fetch loop feeding analytics and the index
    analytics.rs          Chain analytics aggregation
    analytics_streaming.rs The analytics engine as a chain-stream sink
    index/                The address index (fjall store, writer, queries) and its writer task
    watch.rs              Watchlist, UtxosChanged events and alert rules
    labels.rs             Address labels (user, kas.fyi, api.kaspa.org, KNS, heuristics)
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
      monitoring.rs       Monitoring tab: watchlist, alerts, activity
      settings.rs         Settings page: address label sources and known labels
      address.rs          Address Info window
      flows.rs            Flow graph window
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
    address.rs index.rs   `address …` queries and the headless `index run` / `index status`
    watch.rs labels.rs    `watch` event stream and `labels …`
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
