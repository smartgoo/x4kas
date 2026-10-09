//! Chain analytics cards on the Dashboard, modeled on the Kaspalytics home page. Data
//! comes from the analytics task, which needs a direct node.

use eframe::egui::{self, RichText, Ui, layers::ShapeIdx, scroll_area::ScrollAreaOutput};
use egui_extras::{Column, TableBuilder};

use super::theme;
use super::widgets::{
    address, card_with_header, direct_node_placeholder, fit_label, kv, kv_columns, kv_grid,
    kv_with, or_dash, placeholder, request_address, section_title, subheader,
};
use x4kas_core::analytics::{AggregatedView, InspectionCounts, ScriptClassCounts};
use x4kas_core::app::{AnalyticsPanel, App, ChainPhase, TimeWindow};
use x4kas_core::format::{format_kas, format_number};
use x4kas_core::tx_inspect::TransactionProtocol;

/// Tables taller than this scroll.
/// A [`wide_table`]'s body height.
pub const TABLE_HEIGHT: f32 = 200.0;

/// Shown instead of an empty list while analytics catches up, when its counts are partial.
const SYNCING: &str = "Analyzing DAG…";

/// What to show instead of an empty list while there is no view: why there is none,
/// from the chain pipeline's phase (an error is not "analyzing").
fn waiting_text(app: &App) -> &'static str {
    match app.chain.phase {
        ChainPhase::Error(_) => "Analyzer error (see the status bar)",
        ChainPhase::WaitingForNode => "Waiting for the node to sync…",
        ChainPhase::Idle | ChainPhase::Opening => "Starting the analyzer…",
        _ if app.chain.write_error.is_some() => "Analyzer error (see the status bar)",
        _ => SYNCING,
    }
}

/// A count from `view`, or a dash without one (while syncing), never a partial `0`.
fn count(view: Option<&AggregatedView>, f: impl FnOnce(&AggregatedView) -> u64) -> String {
    or_dash(view.map(f), format_number)
}

/// Catch-up progress in `0.0..=1.0` while the chain stream is syncing to the tip.
fn sync_fraction(app: &App) -> Option<f32> {
    let status = &app.chain;
    let tip = app.node.server_info.as_ref()?.virtual_daa_score;
    matches!(status.phase, ChainPhase::Seeking | ChainPhase::CatchingUp)
        .then(|| status.fraction(tip).unwrap_or(0.0))
}

/// One pulse of [`sync_dot`], in seconds.
const PULSE_SECS: f64 = 1.6;

/// A pulsing orange dot while the chain stream catches up to the tip, the same color as
/// the status bar's chain indicator. The progress shows on hover. Goes in a card header.
pub(super) fn sync_dot(ui: &mut Ui, app: &App) {
    let Some(fraction) = sync_fraction(app) else {
        return;
    };
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let phase = ui.input(|i| i.time) / PULSE_SECS * std::f64::consts::TAU;
    let pulse = 0.5 + 0.5 * phase.sin() as f32;
    ui.painter().circle_filled(
        rect.center(),
        size * 0.25,
        theme::WARN.gamma_multiply(0.3 + 0.7 * pulse),
    );
    response.on_hover_text(format!("Analyzing DAG ({:.0}%)", fraction * 100.0));
    // Animate only while syncing; the frame loop otherwise repaints once a second.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(50));
}

/// A card for an analytics panel, with its time window dropdown after the title. The
/// contents get the view for that window. Until there is one, and while analytics catches
/// up to the tip (partial counts), they get none and draw their skeleton: every row with
/// dashes and lists with [`SYNCING`], so the card keeps its size as data arrives. A
/// [`sync_dot`] pulses after the dropdown while syncing. Analytics needs a direct node.
pub(super) fn panel_card(
    ui: &mut Ui,
    app: &mut App,
    title: &str,
    panel: AnalyticsPanel,
    add_contents: impl FnOnce(&mut Ui, &App, Option<&AggregatedView>),
) {
    card_with_header(
        ui,
        title,
        app,
        |ui, app| {
            if !app.connection.is_direct() {
                return;
            }
            let window = app.analytics.window_mut(panel);
            egui::ComboBox::from_id_salt(("time_window", format!("{panel:?}")))
                .selected_text(window.label())
                .width(0.0)
                .show_ui(ui, |ui| {
                    for w in TimeWindow::ALL {
                        ui.selectable_value(window, w, w.label());
                    }
                })
                .response
                .on_hover_text("Time window of this card");
            sync_dot(ui, app);
        },
        |ui, app| {
            if !app.connection.is_direct() {
                placeholder(ui, direct_node_placeholder(app, ""));
                return;
            }
            let view = app.analytics.view(app.analytics.window(panel));
            add_contents(ui, app, view.filter(|_| sync_fraction(app).is_none()));
        },
    );
}

/// Analytics notices (e.g. a reorg) above the Dashboard cards.
pub(super) fn banners(ui: &mut Ui, app: &mut App) {
    let mut dismiss = false;
    if let Some(ref msg) = app.analytics.reorg_notification {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("⚠ {msg}"))
                    .strong()
                    .color(theme::WARN),
            );
            dismiss = ui.button("Dismiss").clicked();
        });
        ui.add_space(2.0);
    }
    if dismiss {
        app.analytics.reorg_notification = None;
    }
}

// ── Transactions chart ──

/// Height of the transactions bar chart.
const TX_CHART_HEIGHT: f32 = 48.0;

/// Transactions per 10 minutes over the last 24h as a thin full-width bar chart, scaled
/// to the busiest interval. The newest bar, still filling, is dimmed. Hovering a bar
/// shows its interval and count. Draws as data arrives, also while analytics catches up.
pub(super) fn tx_chart(ui: &mut Ui, app: &App) {
    if !app.connection.is_direct() {
        placeholder(ui, direct_node_placeholder(app, ""));
        return;
    }
    let size = egui::vec2(ui.available_width(), TX_CHART_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let baseline = rect.bottom() - 1.0;
    painter.hline(
        rect.x_range(),
        baseline + 0.5,
        egui::Stroke::new(1.0_f32, theme::BORDER_HI),
    );
    let histogram = app
        .analytics
        .tx_histogram
        .as_ref()
        .filter(|h| h.counts.iter().any(Option::is_some));
    let Some(histogram) = histogram else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            waiting_text(app),
            egui::TextStyle::Body.resolve(ui.style()),
            theme::TEXT_DIM,
        );
        return;
    };

    let n = histogram.counts.len();
    let max = histogram
        .counts
        .iter()
        .flatten()
        .max()
        .copied()
        .unwrap_or(0)
        .max(1);
    let slot = rect.width() / n as f32;
    let gap = if slot >= 3.0 { 1.0 } else { 0.0 };
    let hovered = response
        .hover_pos()
        .map(|p| (((p.x - rect.left()) / slot) as usize).min(n - 1));
    for (i, count) in histogram.counts.iter().enumerate() {
        let left = rect.left() + i as f32 * slot;
        let column = egui::Rect::from_x_y_ranges(left..=left + slot - gap, rect.top()..=baseline);
        if hovered == Some(i) {
            painter.rect_filled(column, 0.0, theme::ROW_HOVER);
        }
        let Some(count) = *count else {
            continue;
        };
        // At least a sliver for any transactions, so a quiet interval isn't mistaken for an empty one.
        let mut height = count as f32 / max as f32 * column.height();
        if count > 0 {
            height = height.max(1.0);
        }
        let color = if hovered == Some(i) {
            theme::ACCENT_BRIGHT
        } else if i == n - 1 {
            theme::ACCENT_DIM
        } else {
            theme::ACCENT
        };
        let bar = egui::Rect::from_x_y_ranges(column.x_range(), baseline - height..=baseline);
        painter.rect_filled(bar, 0.0, color);
    }

    if let Some(i) = hovered {
        response.on_hover_ui_at_pointer(|ui| {
            let start = histogram.start_ms + i as u64 * histogram.interval_ms;
            let end = start + histogram.interval_ms;
            let mut interval = format!("{}–{} UTC", utc_time(start), utc_time(end));
            if i == n - 1 {
                interval.push_str(" (filling)");
            }
            kv_grid(ui, "tx_chart_tip", |ui| {
                kv(ui, "Interval", interval);
                kv(
                    ui,
                    "Transactions",
                    or_dash(histogram.counts[i], format_number),
                );
            });
        });
    }
}

/// `ms` since the Unix epoch as a UTC time of day, e.g. `14:30`.
fn utc_time(ms: u64) -> String {
    let mins = ms / 60_000 % (24 * 60);
    format!("{:02}:{:02}", mins / 60, mins % 60)
}

// ── Transaction Summary ──

pub(super) fn tx_summary(ui: &mut Ui, _app: &App, view: Option<&AggregatedView>) {
    kv_columns(ui, 220.0, |[txs, classes]| {
        subheader(txs, "Unique Transactions");
        kv_grid(txs, "tx_summary", |ui| {
            kv(ui, "Tx Count", count(view, |v| v.totals.tx_count));
            let tps = view.and_then(AggregatedView::tps);
            kv(ui, "TPS", or_dash(tps, |tps| format!("{tps:.2}")));
            kv(ui, "Chain Blocks", count(view, |v| v.totals.chain_blocks));
        });
        subheader(classes, "Output Script Classes");
        let c = |f: fn(&ScriptClassCounts) -> u64| count(view, |v| f(&v.totals.script_classes));
        kv_grid(classes, "script_classes", |ui| {
            kv(ui, "P2PK", c(|c| c.pubkey));
            kv(ui, "P2PK ECDSA", c(|c| c.pubkey_ecdsa));
            kv(ui, "P2SH", c(|c| c.script_hash));
            kv(ui, "Non-standard", c(|c| c.nonstandard));
        });
    });
}

// ── Fees ──

/// Accepted fees per time window, like Kaspalytics: averages in `avg`, totals in
/// `total` (the Fees card's second and third columns).
pub(super) fn fee_windows(avg: &mut Ui, total: &mut Ui, app: &App) {
    subheader(avg, "Average Fee (KAS)");
    subheader(total, "Total Fees (KAS)");
    if !app.connection.is_direct() {
        let msg = direct_node_placeholder(app, "");
        placeholder(avg, msg);
        placeholder(total, msg);
        return;
    }
    // Dashes until there are views, and while syncing rather than partial sums.
    let syncing = sync_fraction(app).is_some();
    let views = || {
        TimeWindow::ALL.into_iter().map(move |w| {
            let view = app.analytics.view(w).filter(|_| !syncing);
            (format!("Prior {}", w.label()), view)
        })
    };
    kv_grid(avg, "avg_fees", |ui| {
        for (label, view) in views() {
            let fee = view.and_then(AggregatedView::avg_fee);
            kv(ui, &label, or_dash(fee, |f| format_kas(f, 6)));
        }
    });
    kv_grid(total, "total_fees", |ui| {
        for (label, view) in views() {
            let fees = view
                .map(|v| &v.totals)
                .and_then(|t| (t.fee_tx_count > 0).then_some(t.total_fees));
            kv(ui, &label, or_dash(fees, |f| format_kas(f as f64, 8)));
        }
    });
}

// ── Transaction Inspection ──

pub(super) fn inspection(ui: &mut Ui, _app: &App, view: Option<&AggregatedView>) {
    let i = |f: fn(&InspectionCounts) -> u64| count(view, |v| f(&v.totals.inspection));
    kv_columns(ui, 270.0, |cols: &mut [Ui; 3]| {
        subheader(&mut cols[0], "Opcodes");
        kv_grid(&mut cols[0], "inspect_opcodes", |ui| {
            kv(ui, "Introspection Txs", i(|i| i.introspection_txs));
            kv(ui, "OpZkPrecompile Txs", i(|i| i.zk_precompile_txs));
            kv(ui, "└ Groth16", i(|i| i.zk_groth16_txs));
            kv(ui, "└ R0Succinct", i(|i| i.zk_r0succinct_txs));
            kv(
                ui,
                "OpChainblockSeqCommit Txs",
                i(|i| i.chainblock_seqcommit_txs),
            );
        });

        subheader(&mut cols[1], "Covenants");
        kv_grid(&mut cols[1], "inspect_covenants", |ui| {
            kv(ui, "Covenant-Creating Txs", i(|i| i.covenant_creating_txs));
            kv(ui, "Outputs Created", i(|i| i.covenant_outputs_created));
            kv(ui, "Outputs Spent", i(|i| i.covenant_outputs_spent));
        });

        subheader(&mut cols[2], "Protocols");
        kv_grid(&mut cols[2], "inspect_protocols", |ui| {
            for p in TransactionProtocol::ALL {
                kv(ui, p.label(), count(view, |v| v.protocol_count(p)));
            }
        });
    });
}

// ── Mining Share by Node Version ──

pub(super) fn node_versions(ui: &mut Ui, app: &App, view: Option<&AggregatedView>) {
    let total = view.map_or(0, AggregatedView::node_version_total);
    let share = |n: u64| n as f64 / total as f64 * 100.0;
    let name = |v: &str| if v.is_empty() { "Unknown" } else { v }.to_string();

    let rows = view
        .map(|v| v.node_versions.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|(v, n)| [name(v), format_number(*n), format!("{:.2}%", share(*n))])
        .collect();
    wide_table(
        ui,
        "node_versions",
        ["Version", "Blocks", "Share"],
        rows,
        if view.is_some() {
            "No coinbase data yet"
        } else {
            waiting_text(app)
        },
        |ui, v| {
            fit_label(ui, v);
        },
        None,
    );
    ui.add_space(2.0);
    let footnote = match view {
        Some(view) => format!(
            "From {} accepted coinbase transactions.",
            format_number(view.node_version_total())
        ),
        None => "Counting coinbase transactions…".to_string(),
    };
    ui.label(RichText::new(footnote).weak());
}

// ── Top Senders / Receivers ──

/// `entries` is `None` while syncing.
pub(super) fn addresses(ui: &mut Ui, app: &App, entries: Option<&[(String, u64)]>, kind: &str) {
    let empty = match entries {
        Some(_) => format!("No {kind} data yet"),
        None => waiting_text(app).to_string(),
    };
    let rows = entries
        .unwrap_or_default()
        .iter()
        .map(|(addr, n)| [addr.clone(), format_number(*n)])
        .collect();
    wide_table(
        ui,
        &format!("{kind}s"),
        ["Address", "Txs"],
        rows,
        &empty,
        address,
        Some(request_address),
    );
}

/// Full-width table: the first column takes the remaining width and is drawn by
/// `first_cell`, which fits it to that width (e.g. [`fit_label`], [`address`]). The other
/// columns are right-aligned, so values sit against the right edge of the card. The body
/// is always [`TABLE_HEIGHT`] tall (scrolling beyond), with `empty` in its first row
/// while there are no rows, so the card keeps its size as rows arrive.
pub fn wide_table<const N: usize>(
    ui: &mut Ui,
    id: &str,
    headers: [&str; N],
    rows: Vec<[String; N]>,
    empty: &str,
    first_cell: fn(&mut Ui, &str),
    row_click: Option<fn(&egui::Context, &str)>,
) {
    wide_table_with_lead(
        ui,
        id,
        headers,
        rows,
        empty,
        first_cell,
        row_click,
        |_, _| {},
        TABLE_HEIGHT,
    );
}

/// [`wide_table`] with `lead` drawn at the start of every row (given its index), before
/// the first cell: a marker such as the Activity card's alert dot. It should take the
/// same width on every row so the first cells line up. The body is `height` tall: the
/// Monitoring tab's panes give their tables the rest of the tab.
#[allow(clippy::too_many_arguments)]
pub fn wide_table_with_lead<const N: usize>(
    ui: &mut Ui,
    id: &str,
    headers: [&str; N],
    rows: Vec<[String; N]>,
    empty: &str,
    first_cell: fn(&mut Ui, &str),
    // A click anywhere else on a row (the rows highlight as one) acts on its first
    // cell's value, e.g. opens the address's info pane.
    row_click: Option<fn(&egui::Context, &str)>,
    mut lead: impl FnMut(&mut Ui, usize),
    height: f32,
) {
    ui.push_id(id, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        // Room for clickable cells such as [`address`]: a selectable label is at least
        // `interact_size.y` tall and grows by `expansion` on hover, and the next row paints
        // over anything that spills past this one.
        let row_height =
            ui.spacing().interact_size.y + 2.0 * ui.visuals().widgets.hovered.expansion;
        // Exact widths, recomputed every frame: a `Column::remainder` never shrinks below
        // what its content used last frame, so a fitted first column would only ever grow
        // and never shorten its values when the window narrows. All text is monospace, so
        // the other columns are sized by their longest cell.
        let font = egui::TextStyle::Body.resolve(ui.style());
        let glyph = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let spacing = ui.spacing().item_spacing.x;
        let widths: Vec<f32> = (1..N)
            .map(|i| {
                let chars = rows
                    .iter()
                    .map(|r| r[i].chars().count())
                    .chain([headers[i].chars().count()])
                    .max()
                    .unwrap_or(0);
                (chars as f32 * glyph).max(60.0)
            })
            .collect();
        let rest: f32 = widths.iter().map(|w| w + spacing).sum();
        let first = (ui.available_width() - rest).max(0.0);
        let ctx = ui.ctx().clone();
        let layer = ui.layer_id();
        let bounce_id = ui.id().with("overscroll");
        overscroll_unwind(ui, bounce_id);
        let mut table = TableBuilder::new(ui)
            .striped(true)
            // Interactive cells, so egui_extras highlights the hovered row.
            .sense(egui::Sense::click())
            .min_scrolled_height(height)
            .max_scroll_height(height)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(first));
        for w in widths {
            table = table.column(Column::exact(w));
        }
        let table = table.header(theme::ROW_HEIGHT, |mut header| {
            for (i, h) in headers.into_iter().enumerate() {
                header.col(|ui| {
                    right_after_first(ui, i, |ui| section_title(ui, h));
                });
            }
        });
        // The body's shapes, to shift them while bouncing.
        let next_shape = || ctx.graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
        let first_shape = next_shape();
        let output = table.body(|mut body| {
            if rows.is_empty() {
                body.row(row_height, |mut row| {
                    row.col(|ui| placeholder(ui, empty));
                    for _ in 1..N {
                        row.col(|_| {});
                    }
                });
                return;
            }
            body.rows(row_height, rows.len(), |mut row| {
                let index = row.index();
                for (i, cell) in rows[index].iter().enumerate() {
                    row.col(|ui| {
                        if i == 0 {
                            lead(ui, index);
                            first_cell(ui, cell);
                        } else {
                            right_after_first(ui, i, |ui| {
                                ui.label(cell);
                            });
                        }
                    });
                }
                if let Some(on_click) = row_click
                    && row.response().clicked()
                {
                    on_click(&ctx, &rows[index][0]);
                }
            });
        });
        let bounce = overscroll(ui, bounce_id, &output);
        if bounce != 0.0 {
            let shapes = first_shape..next_shape();
            ctx.graphics_mut(|g| {
                let list = g.entry(layer);
                for i in shapes {
                    // Only the shapes move: their clip rect stays the body's, so rows
                    // don't spill over the header or out of the card.
                    list.mutate_shape(ShapeIdx(i), |s| s.shape.translate(egui::vec2(0.0, bounce)));
                }
            });
        }
    });
}

/// Rubber-band stiffness, as in UIScrollView: a pull of `x` past the end shows as
/// `(1 - 1 / (x * C / d + 1)) * d` for a viewport `d` tall, so it gets harder the further
/// it goes and never reaches `d`.
const RUBBER_BAND_C: f32 = 0.55;
/// Pulled rows are released once scrolling has stopped for this long, in seconds.
const RELEASE_AFTER: f64 = 0.06;
/// Angular frequency of the critically damped spring back (2π / ~0.4s response).
const SPRING_OMEGA: f32 = 16.0;

/// A table's overscroll, between frames.
#[derive(Clone, Copy, Default)]
struct Overscroll {
    /// Rows' displayed offset past the end, in points (positive: pulled down at the top).
    offset: f32,
    /// Spring velocity once released, in points per second.
    velocity: f32,
    /// When scrolling last pulled the rows.
    last_pull: f64,
    /// Last frame's body viewport, to catch scrolling back before the table does.
    viewport: Option<egui::Rect>,
}

/// The most a table's rows can be pulled past an end, in points, however fast the scroll.
const MAX_STRETCH: f32 = 36.0;

/// The rubber band's `d`: what the stretch approaches but never reaches. The viewport's
/// height, as in UIScrollView, but bounded, so a fast flick can't pull the rows far. The
/// stretch starts out just as stiff either way (its slope at rest is `C`).
fn stretch_limit(viewport: egui::Rect) -> f32 {
    viewport.height().clamp(1.0, MAX_STRETCH)
}

fn rubber_band(pull: f32, d: f32) -> f32 {
    (1.0 - 1.0 / (pull.abs() * RUBBER_BAND_C / d + 1.0)) * d * pull.signum()
}

/// Inverse of [`rubber_band`]: the pull that shows as `offset`.
fn rubber_band_pull(offset: f32, d: f32) -> f32 {
    let o = offset.abs().min(d * 0.99);
    d / RUBBER_BAND_C * (1.0 / (1.0 - o / d) - 1.0) * offset.signum()
}

/// Before the table: while its rows are pulled, scrolling back toward the content first
/// unwinds the pull (anything past that scrolls the table as usual), like iOS.
fn overscroll_unwind(ui: &Ui, id: egui::Id) {
    let mut state: Overscroll = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let Some(viewport) = state.viewport.filter(|_| state.offset != 0.0) else {
        return;
    };
    if !ui.rect_contains_pointer(viewport) {
        return;
    }
    let delta = ui.input(|i| i.smooth_scroll_delta.y);
    if delta == 0.0 || delta.signum() == state.offset.signum() {
        return;
    }
    let d = stretch_limit(viewport);
    let pull = rubber_band_pull(state.offset, d);
    let unwound = pull + delta;
    let rest = if unwound.signum() == pull.signum() {
        state.offset = rubber_band(unwound, d);
        0.0
    } else {
        state.offset = 0.0;
        unwound
    };
    state.velocity = 0.0;
    state.last_pull = ui.input(|i| i.time);
    ui.ctx().input_mut(|i| i.smooth_scroll_delta.y = rest);
    ui.data_mut(|d| d.insert_temp(id, state));
}

/// After the table: while the pointer is over a table that scrolls, the page under it
/// doesn't. Scrolling past the table's top or bottom pulls its rows with iOS-style
/// rubber-band resistance; once scrolling stops they spring back. Returns the rows'
/// offset this frame.
fn overscroll(ui: &Ui, id: egui::Id, output: &ScrollAreaOutput<()>) -> f32 {
    let mut state: Overscroll = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    state.viewport = Some(output.inner_rect);
    let d = stretch_limit(output.inner_rect);
    let now = ui.input(|i| i.time);
    let scrollable = output.content_size.y > output.inner_rect.height() + 0.5;
    if scrollable && ui.rect_contains_pointer(output.inner_rect) {
        // What the table's own scroll area left over: it is at an end.
        let leftover = ui
            .ctx()
            .input_mut(|i| std::mem::take(&mut i.smooth_scroll_delta.y));
        if leftover != 0.0 {
            let pull = rubber_band_pull(state.offset, d) + leftover;
            state.offset = rubber_band(pull, d);
            state.velocity = 0.0;
            state.last_pull = now;
        }
    }
    if state.offset != 0.0 && now - state.last_pull > RELEASE_AFTER {
        // Critically damped spring back to rest: no overshoot, like iOS.
        let dt = ui.input(|i| i.stable_dt).min(1.0 / 30.0);
        let accel =
            -SPRING_OMEGA * SPRING_OMEGA * state.offset - 2.0 * SPRING_OMEGA * state.velocity;
        state.velocity += accel * dt;
        state.offset += state.velocity * dt;
        if state.offset.abs() < 0.1 && state.velocity.abs() < 5.0 {
            state = Overscroll {
                viewport: state.viewport,
                ..Default::default()
            };
        }
    }
    if state.offset != 0.0 {
        ui.ctx().request_repaint();
    }
    ui.data_mut(|d| d.insert_temp(id, state));
    state.offset
}

/// Cells after the first are pinned to the right edge of their column, so values line up
/// with the right edge of the card.
fn right_after_first(ui: &mut Ui, column: usize, add: impl FnOnce(&mut Ui)) {
    if column == 0 {
        add(ui);
    } else {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
    }
}

// ── Mining ──

/// Miner counts over the Top Miners card's time window, as rows of the Mining
/// card's [`kv_grid`]. Dashes with the resolver and while analytics catches up.
pub(super) fn miner_counts(ui: &mut Ui, app: &App) {
    let window = app.analytics.window(AnalyticsPanel::Miners);
    let view = app
        .analytics
        .view(window)
        .filter(|_| app.connection.is_direct() && sync_fraction(app).is_none());
    let w = window.label();
    const FOLLOWS: &str = "Over the Top Miners card's time window";
    kv_with(ui, &format!("Unique Miners ({w})"), |ui| {
        ui.label(count(view, |v| v.unique_miners as u64))
            .on_hover_text(FOLLOWS);
    });
    kv_with(ui, &format!("Blocks Mined ({w})"), |ui| {
        ui.label(count(view, |v| v.totals.mined_blocks))
            .on_hover_text(FOLLOWS);
    });
}

pub(super) fn top_miners(ui: &mut Ui, app: &App, view: Option<&AggregatedView>) {
    let blocks = view.map_or(0, |v| v.totals.mined_blocks);
    let share = |n: u64| n as f64 / blocks.max(1) as f64 * 100.0;
    let rows = view
        .map(|v| v.top_miners.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|(addr, n)| {
            [
                addr.clone(),
                format_number(*n),
                format!("{:.2}%", share(*n)),
            ]
        })
        .collect();
    wide_table(
        ui,
        "top_miners",
        ["Miner", "Blocks", "Share"],
        rows,
        if view.is_some() {
            "No miner data yet"
        } else {
            waiting_text(app)
        },
        address,
        Some(request_address),
    );
}
