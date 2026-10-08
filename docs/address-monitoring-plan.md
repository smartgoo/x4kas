# Address Monitoring: Design and Implementation Plan

Status: implemented (Phases 0–5; see "Outcome" at the end) · Date: 2026-10-07, updated 2026-10-08 · Scope: x4kas GUI and CLI, `x4kas-core`

## 1. Goal

Give x4kas address intelligence on par with the best tools on other networks
(Arkham, Nansen, Chainalysis Reactor, OXT) for the window the connected node
retains, with no external indexer:

- **Watch**: a watchlist with sub-second alerts on balance changes, incoming and
  outgoing payments and thresholds, in the GUI and as a CLI stream.
- **Profile**: any address, on click: label, balance, UTXO set, balance history,
  transactions, counterparties, first/last seen, cluster membership.
- **Relate**: group addresses into likely owners (common-input ownership, change
  detection), label them (public label services + user labels), and follow funds
  hop by hop in an interactive flow graph.
- **Scale**: sustain the throughput Kaspa is built for (thousands of TPS) without
  the GUI stalling, with bounded memory and disk.

Initial scope is activity from the node's pruning point forward. Deep history
(archive nodes, external indexers) is out of scope for this plan; the design keeps
the door open (§10).

## 2. Constraints and facts the design rests on

- **Data source.** Everything comes from the connected node (project rule). The
  VSPC v2 stream (`get_virtual_chain_from_block_v2`, verbosity `High`,
  `min_confirmation_count = 10`) already drives analytics and gives, per accepted
  transaction: the transaction id, accepting chain block (hash, DAA score,
  timestamp), every input's spent UTXO (amount, script, **address**), every output
  (amount, **address**), payload and mass. That is the complete input to an
  address index; nothing else needs to be fetched during ingest.
- **Batching.** One VSPC v2 response covers up to `mergeset_size_limit × 10` =
  2,480 chain blocks on mainnet (≈ 4 minutes of chain at 10 bps), so catch-up is a
  few hundred requests a day of chain, and live polling at 1 s drains the tip in
  one request.
- **Retention.** Mainnet `pruning_depth` is 1,080,000 blocks ≈ 30 h at 10 bps;
  `kaspad --retention-period-days N` extends what the node keeps. Our index covers
  exactly what the node can serve and prunes in step with it.
- **Live address data.** With `--utxoindex` the node provides
  `get_balances_by_addresses`, `get_utxos_by_addresses`,
  `get_mempool_entries_by_addresses`, `get_utxo_return_address`, and the
  `UtxosChanged` notification (added/removed UTXOs per subscribed address). This
  is the alert path; it is independent of the index and needs no catch-up.
- **Direct node only.** Like analytics, the index runs only with a URL connection
  (`ActiveConnection::is_direct`). The watchlist can run through the resolver as
  well when the resolved node reports `has_utxo_index`.
- **Threading.** The GUI never awaits and never touches disk; it sends `UiCommand`s
  and reads views from `App`. Every background task is tracked in
  `PollingHandles`. Background writers call `mark_dirty()`.
- **Public label services (verified 2026-10-07).**
  - `GET https://api.kaspa.org/addresses/names`: the whole list (138 entries
    today: exchanges, pools, funds, bridges, burn address), no key, and
    `GET /addresses/{address}/name` for one. Free and bulk: fetching the list
    doesn't reveal which addresses the user looks at.
  - `GET https://api.kas.fyi/v1/addresses/{address}/tag` with an `x-api-key`
    (free key at developer.kas.fyi): `{tag: {address, name, link, labels[]}}`,
    404 when unknown, `labels` are clustering categories such as `exchange`.
    Per-address, so it leaks the queried address to kas.fyi.
  - KNS (`.kas` names): `https://api.knsdomains.org/mainnet/api/v1/domain/{domain}`
    and `…/assets?owner={address}` (reverse). Endpoint shapes still to be
    confirmed against `apidoc.knsdomains.org` before implementation.

## 3. Throughput budget

Target: ingest **≥ 5,000 TPS sustained** on a laptop (headroom over Kaspa's
~3,000 TPS), with the GUI at 60 fps throughout.

At 3,000 TPS and ~2 inputs + ~2 outputs per transaction:

| Load | Tx/day | Address touches/s | Index growth/day (est.) |
|---|---|---|---|
| 100 TPS (today's order of magnitude) | 8.6 M | ~400 | ~1 GB |
| 1,000 TPS | 86 M | ~4,000 | ~10 GB |
| 3,000 TPS | 259 M | ~12,000 | ~30 GB |

Growth estimates assume ~100–120 bytes per transaction after key-prefix
compression (§5.3). Disk use is bounded by the retention window (30 h default),
not by uptime, because slabs are dropped as the node prunes. The node itself holds
a similar order of data for the same window, so this is proportionate.

Catch-up is bounded by the node's RPC throughput, not ours: a full 30 h window at
3,000 TPS is 324 M transactions. The index therefore backfills a configurable
window (default: whatever the node retains; `index.backfill_hours` to cap it) and
shows an ETA, exactly like analytics' progress today.

## 4. Architecture

```
                 ┌──────────────────────────────────────────────────────────┐
  node (wRPC)    │ x4kas-core                                               │
  ───────────────┼─▶ chain_stream (VSPC v2 fetcher, one per connection)     │
                 │      │ Arc<GetVirtualChainFromBlockV2Response>           │
                 │      ├─▶ analytics sink  (existing engine, unchanged)    │
                 │      └─▶ index sink      (bounded channel) ─▶ writer     │
                 │                                   thread ─▶ fjall store  │
  UtxosChanged ──┼─▶ watch task ─▶ alerts + watchlist state in App           │
  CoinGecko ─────┼─▶ (existing)                                             │
  label APIs ────┼─▶ labels task (bulk list refresh, on-demand lookups)     │
                 │                                                          │
                 │ queries: AddressQuery (spawn_blocking reads) ─▶ App views│
                 └──────────────────────────────────────────────────────────┘
                     ▲ UiCommand                         │ views, mark_dirty
                 ┌───┴──────────┐                   ┌────▼────────┐
                 │ x4kas (egui) │                   │ x4kas-cli   │
                 └──────────────┘                   └─────────────┘
```

Principles:

1. **One fetch, many sinks.** The VSPC v2 stream is fetched once per connection
   and fanned out to analytics and the index. Today `analytics_streaming::run`
   owns the fetch loop; it becomes `chain_stream.rs` with a `ChainSink` trait and
   the analytics engine as its first sink. No second VSPC poller.
2. **Backpressure, never unbounded buffering.** A bounded channel (a handful of
   responses) sits between fetcher and writer. If the writer falls behind, the
   fetcher waits; nothing is dropped and memory stays flat.
3. **Disk work off the runtime.** The store is synchronous; writes happen on one
   dedicated writer thread, reads via `tokio::task::spawn_blocking`. The GUI thread
   never opens the store.
4. **Alerts don't wait for the index.** `UtxosChanged` fires as soon as the node's
   UTXO index changes (before the 10-confirmation VSPC lag), so watchlist alerts
   are live even while the index is still catching up.
5. **Core owns the logic, frontends render.** Profiles, clusters, flows and label
   resolution are core APIs returning plain structs; the GUI draws them and the
   CLI prints them as JSON.

## 5. Address index (`x4kas-core::index`)

### 5.1 Storage engine

**fjall 3.x** (pure Rust LSM-tree: keyspaces as column families, atomic write
batches, block cache, compression, range deletes). Rationale:

- Write-optimized: random-key inserts at >10k/s with batched writes are exactly
  the LSM sweet spot; a B-tree store (redb) pays write amplification on random
  address keys at this rate.
- Pure Rust: keeps `x4kas-cli` building fast on servers (no C++ toolchain, unlike
  `rocksdb`).
- Keyspaces let us partition by time (§5.3), making pruning O(1).

The store sits behind a small `IndexStore` trait (open, write batch, point get,
prefix scan, drop slab) so the engine can be swapped if the spike in Phase 0
(§8) disappoints. RocksDB is the fallback.

Location: `~/.x4kas/index/<network>/`. A `MANIFEST` key holds a format version
(bump to discard, as `CACHE_MAGIC` does), the network, and the last indexed chain
block hash and DAA score for resume.

### 5.2 Records

Addresses are interned to `u32` ids (`addr_by_str`, `str_by_id` keyspaces) so
every other key is small and fixed-width. Transaction ids stay 32 bytes.

Per transaction (`tx` keyspace, key = txid):

```
IndexedTx {
  accepting_block: hash32, daa: u64, time_ms: u64,
  inputs:  [(addr_id u32, amount u64, prev_txid hash32, prev_index u32)],
  outputs: [(addr_id u32, amount u64)],
  fee: u64, mass: u64, is_coinbase: bool, protocol: u8 (tx_inspect),
}
```

encoded with `bincode` (varints, fixed layout; no JSON on the hot path).

### 5.3 Keyspaces and key layout

Keys are designed so every query is one point get or one prefix scan, and every
prune is a slab drop.

| Keyspace | Key | Value | Purpose |
|---|---|---|---|
| `tx` (slabbed) | `txid` | `IndexedTx` | transaction detail |
| `addr_tx` (slabbed) | `addr_id ‖ time_ms ‖ txid` | `delta: i64` | an address's transactions in time order, newest-first scan |
| `block_tx` (slabbed) | `block_hash ‖ txid` | `()` | reorg undo (which txs a chain block accepted) |
| `addr_stats` | `addr_id` | `{first_seen, last_seen, tx_count, received, sent, in_cluster: u32}` | profile header without scanning |
| `addr_peer` | `addr_id ‖ peer_id` | `{volume_in, volume_out, count}` | counterparties |
| `cluster` | `cluster_id` | `{members: Vec<u32>, root: u32}` | union-find result (§6.2) |
| `addr_by_str` / `str_by_id` | address / id | id / address | interning |

"Slabbed" keyspaces are opened per **6-hour slab** (`tx@<slab_no>`,
`addr_tx@<slab_no>`, …), slab number = `time_ms / 6h`. Prefix compression in the
LSM makes the shared `addr_id` prefix nearly free. A query over the 30 h window
merges at most 6 slab scans. Pruning drops whole slabs once the node's pruning
point passes them; `addr_stats`/`addr_peer` are corrected lazily (counts are
stored per slab inside the value, so dropping a slab subtracts its contribution
without a scan).

### 5.4 Ingest pipeline

1. **Fetch** (async, existing loop): VSPC v2 response → `Arc<Response>` to sinks.
2. **Transform** (writer thread): walk `chain_block_accepted_transactions`, intern
   addresses (in-memory LRU over the `addr_by_str` keyspace), build `IndexedTx`,
   per-address deltas, peer updates, cluster unions. All CPU, no I/O except
   interning misses.
3. **Write**: one atomic batch per VSPC response (thousands of txs), including
   the `MANIFEST` position. Crash = at worst one batch re-applied, which is
   idempotent (same keys, same values).
4. **Reorg**: for each `removed_chain_block_hash`, read `block_tx`, delete those
   txs and their `addr_tx`/peer deltas (reverse-apply), in the same batch. Stats
   are reversed exactly because deltas are stored, not recomputed.
5. **Prune**: after each batch, if the oldest slab is older than the node's
   pruning point timestamp (from `get_block_dag_info` + header), drop it.
6. **Progress**: `app.index.status` mirrors `AnalyticsStatus` (phase, DAA
   position, rate, ETA); the status bar gets an index dot next to the analytics
   dot.

Resume: on connect, read `MANIFEST`; if the node still has that chain block,
continue from it, otherwise ("…retention root…" error) start from the pruning
point, as analytics does. A network mismatch opens a different directory.

### 5.5 Query API (core, sync, called inside `spawn_blocking`)

```rust
pub struct AddressQuery<'a> { store: &'a IndexStore, labels: &'a LabelBook }
impl AddressQuery {
  fn profile(&self, addr) -> AddressProfile        // stats + label + cluster + first/last seen
  fn transactions(&self, addr, page: Cursor) -> Page<TxRow>   // newest first, cursor = (time_ms, txid)
  fn balance_history(&self, addr, now_balance, window) -> Vec<(time_ms, balance)> // replay deltas backwards from the live balance
  fn counterparties(&self, addr, top_n) -> Vec<Peer>
  fn cluster(&self, addr) -> Cluster
  fn flows(&self, roots, hops, top_n) -> FlowGraph   // BFS over addr_peer, aggregated by cluster
  fn transaction(&self, txid) -> Option<IndexedTx>
}
```

Targets: p99 < 50 ms for a profile, < 100 ms for a page of 100 transactions on an
address with 1 M entries (prefix scan from a cursor, no counting), flow graph for
2 hops × top 25 under 500 ms.

## 6. Analysis

### 6.1 Labels (`x4kas-core::labels`)

`LabelBook`: an in-memory map `address → Label { name, source, link, categories }`
with precedence **user > kas.fyi > kaspa.org > KNS > heuristic** (a pool detected
from coinbases, the burn address from `emission::burn_address`).

Sources, each a `LabelSource` with its own cache file under `~/.x4kas/labels/`:

| Source | Mode | Privacy | Default |
|---|---|---|---|
| Bundled snapshot of `api.kaspa.org/addresses/names` + burn addresses | compiled in | none | on |
| `api.kaspa.org/addresses/names` | bulk refresh every 24 h (and on demand) | none (bulk) | on |
| `api.kas.fyi …/tag` | per address on view, cached 7 days (404 cached as negative) | leaks the viewed address | off until the user enters an API key |
| KNS reverse lookup | per address on view, cached 24 h | leaks the viewed address | off; toggle "Resolve .kas names" |
| User labels (`~/.x4kas/labels/user.toml`) | editable in the GUI and CLI | none | on |

Online per-address lookups are an explicit opt-in ("Look up labels online"),
shown in the connection window, because they send addresses the user is
investigating to a third party. Labels show everywhere an address does: a chip
next to `widgets::address`, in tables, on graph nodes.

### 6.2 Clustering (`x4kas-core::cluster`)

Incremental **common-input ownership** with a persistent union-find over
`addr_id`s, updated by the writer as transactions arrive:

- All input addresses of a non-coinbase transaction are unioned.
- Guards against known false merges: skip transactions that spend from two
  addresses carrying different *entity* labels (an exchange sweep never merges
  Binance with Gate.io), skip covenant/multi-party protocols flagged by
  `tx_inspect` (e.g. bridge or DEX contracts), and cap cluster size with a
  warning (a runaway cluster is a heuristic failure, not a discovery).
- A cluster inherits the strongest label of any member; the profile shows
  "likely Gate.io (cluster of 1,204 addresses)".

**Change detection** (scored, never asserted): for each spend, the output that
(a) goes to a previously unseen address, (b) has the same script class as the
inputs, (c) is the non-round amount while the other output is round, or (d) goes
back to an input address, is marked `change: 0.0–1.0`. High-confidence change
outputs join the cluster; the score is shown in the transaction detail.

**Peel chains**: a profile notes when an address is the k-th link of a chain of
single-input two-output spends; the flow graph collapses the chain into one edge
with hop count.

### 6.3 Watchlist and alerts (`x4kas-core::watch`)

- `Watchlist` persisted in `~/.x4kas/watchlist.toml` (addresses, optional name,
  thresholds, enabled rules). Entries are per network.
- A watch task subscribes `UtxosChanged` for all enabled addresses (one
  subscription, resubscribed on connect like `stream_blocks`) and seeds balances
  with `get_balances_by_addresses`. Each notification becomes an `AddressEvent
  { address, kind: Received|Sent|BalanceChanged, amount, txid, time }`; events are
  appended to `app.watch.events` (ring buffer) and matched against alert rules.
- Rules: any activity, received ≥ X, sent ≥ X, balance below/above X, first
  activity after N hours idle. Matches raise an `Alert` shown as a toast and in
  the Alerts feed; desktop notification via the OS later (not in this plan).
- Pending activity: `get_mempool_entries_by_addresses` every poll for watched
  addresses, so "incoming (unconfirmed)" shows before acceptance.
- CLI `x4kas-cli address watch <addr>… --json` streams the same events as JSON
  lines until interrupted.

## 7. Frontends

### 7.1 GUI

- **Addresses tab** (new, `4`): a search box (address or label), the watchlist
  table (name, address, balance, 24 h change, last activity, pending), and the
  Alerts feed. Add/remove/edit from the table; a click opens Address Info.
- **Address Info window** (`gui/address.rs`, opened from any `widgets::address`
  click on any tab, like Block Info): header (label chip, cluster, balance,
  UTXO count, first/last seen, tx count, watch toggle), a balance-over-window
  chart, Transactions (paged `egui_extras` table; txid links to Tx detail,
  addresses link onward), Counterparties, Cluster members, "Open flow graph".
- **Flow graph window**: custom egui painting. Nodes are clusters (or addresses
  when unclustered) sized by volume, labelled; edges are aggregated flows with
  amounts; expand a node by clicking, hop by hop; hover shows the transactions.
  Layout is a simple force-directed pass with pinned roots; no external graph
  crate.
- **Status bar**: an index dot (phase/progress on hover) beside the analytics
  dot; the direct-node placeholder for the resolver.
- All of this is view state in `GuiApp`/egui memory; the data lives in
  `App.address` (`AddressState { open: Option<AddressView>, watch: WatchState,
  index_status: IndexStatus }`) filled by the controller.

New `UiCommand`s: `LookupAddress(String)`, `AddressPage { address, cursor }`,
`AddressFlows { roots, hops }`, `WatchAdd/WatchRemove/WatchUpdate`, `SetLabel`,
`RefreshLabels`. Each is handled with `spawn_rpc`-style tracked tasks that read
the store via `spawn_blocking` and write the result into `App`.

### 7.2 CLI

```
x4kas-cli address <addr>                     # profile as JSON (label, balance, stats, cluster)
x4kas-cli address txs <addr> [--limit --before]
x4kas-cli address peers <addr> [--top N]
x4kas-cli address cluster <addr>
x4kas-cli address flows <addr>… [--hops 2 --top 25]
x4kas-cli address watch <addr>… [--json]     # streams events until Ctrl+C
x4kas-cli labels list|refresh|set <addr> <name>|rm <addr>
x4kas-cli index status|run [--backfill-hours N]   # `run` is the headless indexer for servers
```

`index run` is the piece that makes this usable from scripts and agents on a
server: it opens the store, streams until stopped, and the other commands read
the store it builds. The store has a single-writer lock; the GUI and `index run`
cannot both write the same network directory, and the error says so.

## 8. Phases

Each phase ends green on `cargo fmt --all`, `cargo clippy --all-targets -D
warnings`, `cargo test`, and updates `CLAUDE.md`'s module layout.

**Phase 0: Spike and benchmarks (1 week).** Synthetic VSPC v2 response generator
(`index::bench`), a `criterion` benchmark that ingests 10 minutes of 5,000 TPS
synthetic chain and reports tx/s, bytes/tx on disk, p99 profile and page query
latency on a 1 M-entry address. Decides fjall vs. RocksDB. Deliverable: numbers
in `docs/address-index-bench.md`, go/no-go on the key layout.

**Phase 1: Chain stream + index + CLI (2–3 weeks).** `chain_stream.rs` with the
analytics engine as sink 1 and the index writer as sink 2; keyspaces, records,
resume, reorg undo, slab pruning, `IndexStatus` in the status bar; `x4kas-cli
address|txs|peers`, `index status|run`. Tests: transform of recorded VSPC
responses, reorg reverse-apply equals never-applied, slab drop corrects stats,
resume after kill mid-batch.

**Phase 2: Watchlist and alerts (1–2 weeks).** `watch.rs`, `UtxosChanged`
subscription, rules, Addresses tab, toasts, `address watch`. Works through the
resolver when the node has `--utxoindex`. Tests: rule matching, event building
from notifications, persistence.

**Phase 3: Address Info and labels (2 weeks).** `labels.rs` with the bundled
snapshot, kaspa.org bulk refresh, user labels; the Address Info window with
profile, balance history, transactions, counterparties; label chips on every
`widgets::address`. Tests: precedence, cache expiry, balance replay.

**Phase 4: Clustering, change detection, online labels (2–3 weeks).** Union-find
in the writer, guards, change scores, cluster view; kas.fyi (API key in
settings) and KNS with the opt-in toggle. Tests: heuristics on hand-built
transactions, guard behaviour, cluster size cap.

**Phase 5: Flow graph and export (2 weeks).** `flows()` BFS, graph window,
peel-chain collapsing, CSV/JSON export of transactions and flows, `address
flows` in the CLI.

## 9. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Disk growth at full network load | Slab pruning tied to the node's pruning point; `index.backfill_hours` and a disk cap (`index.max_gb`, oldest slabs dropped first) in settings |
| Catch-up slower than the node prunes (as analytics already handles) | Restart from the pruning point on "retention root" errors; show the gap in the status |
| Writer can't keep up at the tip | Bounded channel makes the fetcher wait; measured in Phase 0; the transform is pure CPU and can move to a second thread if needed |
| Reorgs deeper than the batch | `block_tx` undo handles any depth the node reports; `min_confirmation_count = 10` keeps them rare |
| Clustering false positives (bridges, DEX, covenant protocols) | Entity-label guard, protocol guard from `tx_inspect`, size cap with warning; scores shown, never asserted |
| Online label lookups leak addresses | Off by default; bulk kaspa.org list on by default; explicit toggle with a one-line explanation |
| kas.fyi / KNS API changes or rate limits | Each source is isolated, cached, failure-tolerant; the app works fully without them |
| GUI stalls on disk | No store access on the GUI thread; all reads via `spawn_blocking`, paged with cursors |

## 10. Later (explicitly out of scope now)

Archive history via an external indexer (Kaspa REST API `full-transactions-page`,
kas.fyi) behind the same `AddressQuery` interface; OS notifications and webhooks
for alerts; KRC-20 / Kasplex token balances; exchange-flow dashboards (net
in/outflow per labelled entity) on the Dashboard; importing watchlists and labels
from CSV.

## 11. Decisions requested

1. **fjall as the storage engine**, with RocksDB as the Phase 0 fallback.
2. **Index only with a direct node; watchlist also through the resolver** when
   the node has a UTXO index.
3. **Labels:** kaspa.org bulk list and user labels on by default; kas.fyi and KNS
   per-address lookups opt-in with the privacy note.
4. **A headless `x4kas-cli index run`** sharing the GUI's store format, with a
   single-writer lock.

## 12. Outcome (2026-10-08)

Everything in Phases 0–5 is built, with these deviations from the proposal:

- **Benchmark (Phase 0):** synthetic ingest at 5,000 TPS shape runs at ~80,000 tx/s
  in release mode; storage after compaction is ~280 B/tx (the proposal's 100–120 B
  underestimated per-input outpoints and address churn), so at 3,000 TPS the index
  grows ~70 GB/day; at today's ~100 TPS, ~2.5 GB/day. Profile lookups take tens of
  microseconds and a 100-row page well under a millisecond. fjall was kept; the
  RocksDB fallback was not needed.
- **Backfill:** the GUI streams the last 24 h by default (the Dashboard's window), so
  analytics behave as before; `x4kas-cli index run --backfill-hours 0` indexes the
  whole retention window.
- **Stats layout:** per-address stats and counterparties are slabbed too, so dropping
  a slab removes exactly its contribution; no lazy correction is needed.
- **Peel chains** are not collapsed in the graph yet; a node's hop count and edge
  amounts show them.
- **Alerts** are in-app (toasts over any tab, an unread count on the Addresses tab, the
  Alerts feed) and on the CLI's JSON stream; no OS notifications. The idle rule only
  fires once a previous activity is known (`last_activity_ms`, kept in the watchlist
  file), so the first event after adding an address never counts as "after idle".
- **Online labels:** kas.fyi and KNS lookups are on demand ("Look up" per address),
  never automatic, and cached with negative answers for a week. The opt-in toggles live
  on the Addresses tab (Label Sources), not in the connection window.
- **Heuristic labels** are the burn address of every network and "Mining pool (<tag>)"
  for a payout address once the analytics sink has seen it mine 25 coinbases; they are
  per session (not saved) and sit below every other source.
- **Clustering guards:** the entity guard only counts entity labels (kas.fyi,
  kaspa.org, heuristics), never the user's own notes or `.kas` names, which are
  per address; besides Kasplex and Igra, any spend revealing an introspecting
  redeem script (a covenant) is left unclustered. Refused merges at the size cap are
  counted in the index status (GUI hover, CLI progress line) as the proposal's
  "warning". A cluster is named in core (`ClusterInfo::label`: strongest source,
  then most common name among the sampled members) so the CLI shows it too.
- **Live validation** against a real node was not possible in the build session (no
  node was reachable); the pipeline is covered by synthetic-response tests
  (indexing, replay idempotence, reorg undo, slab pruning, clustering, flows).
