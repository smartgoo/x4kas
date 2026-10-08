//! Small building blocks shared by the tab views.

use std::sync::Arc;

use eframe::egui::{
    self, Button, FontId, Margin, RichText, Stroke, TextEdit, Ui, WidgetText, pos2, text::CCursor,
};

pub use super::analytics::wide_table;
use super::theme;
use x4kas_core::app::{ActiveConnection, App};
use x4kas_core::format::{explorer_address_url, kaspa_stream_address_url, shorten_middle};
use x4kas_core::labels::{Label, LabelBook, LabelSource};
use x4kas_core::rpc::hash_links::HashLink;

/// Vertical space between stacked cards.
pub const CARD_GAP: f32 = 8.0;
/// Horizontal space between cards side by side in a [`weighted_columns`] row.
pub const CARD_GAP_X: f32 = 12.0;
/// Vertical space between [`kv_columns`] that wrapped onto rows inside a card.
const COLUMN_ROW_GAP: f32 = 4.0;

/// A bordered pane with its title set into the top border, like a TUI block:
/// `┌─ Title ───────┐`.
pub fn card(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    card_with_header(ui, title, &mut (), |_, _| {}, |ui, _| add_contents(ui));
}

/// Gutter between [`kv_columns`], so a value never runs into the next column's label.
pub const COLUMN_GAP: f32 = 24.0;

/// A row of cards side by side, like [`egui::Ui::columns`] but with column widths
/// proportional to `weights`, e.g. `[1.0, 2.0]` for a third and two thirds. Each column's
/// [`card`] stretches to the height of its row's tallest card, so their borders line up.
///
/// Responsive, like a web page's flex-wrap: a card that would get narrower than
/// `min_width` wraps onto a new row, and each row shares the full width by its weights.
pub fn weighted_columns<R, const N: usize>(
    ui: &mut Ui,
    weights: [f32; N],
    min_width: f32,
    add_contents: impl FnOnce(&mut [Ui; N]) -> R,
) -> R {
    columns_with_gap(ui, weights, min_width, CARD_GAP_X, true, add_contents)
}

/// `N` equal columns inside a card (e.g. side-by-side [`kv_grid`]s), [`COLUMN_GAP`] apart.
/// Columns narrower than `min_width` wrap onto new rows, like [`weighted_columns`].
pub fn kv_columns<R, const N: usize>(
    ui: &mut Ui,
    min_width: f32,
    add_contents: impl FnOnce(&mut [Ui; N]) -> R,
) -> R {
    columns_with_gap(ui, [1.0; N], min_width, COLUMN_GAP, false, add_contents)
}

/// Splits columns into rows: each row takes columns while every one of them still gets
/// `min_width` of its weighted share. Returns each row's first column index.
fn wrap_rows(weights: &[f32], min_width: f32, width: f32, spacing: f32) -> Vec<usize> {
    let mut starts = vec![0];
    for i in 1..weights.len() {
        let row = &weights[*starts.last().unwrap()..=i];
        let total = row.iter().sum::<f32>();
        let usable = width - spacing * (row.len() as f32 - 1.0);
        if row.iter().any(|w| usable * w / total < min_width) {
            starts.push(i);
        }
    }
    starts
}

/// Last frame's layout of a wrapped [`columns_with_gap`]: where its rows start, their
/// heights (to place the next row) and their cards' natural heights (to stretch them).
#[derive(Clone, Default)]
struct RowLayout {
    starts: Vec<usize>,
    heights: Vec<f32>,
    natural: Vec<f32>,
}

fn columns_with_gap<R, const N: usize>(
    ui: &mut Ui,
    weights: [f32; N],
    min_width: f32,
    spacing: f32,
    stretch_cards: bool,
    add_contents: impl FnOnce(&mut [Ui; N]) -> R,
) -> R {
    let full_width = ui.available_width();
    let starts = wrap_rows(&weights, min_width, full_width, spacing);
    let row_of = |i: usize| starts.iter().rposition(|&s| s <= i).unwrap_or(0);
    let row_range = |r: usize| starts[r]..starts.get(r + 1).copied().unwrap_or(N);
    let top_left = ui.cursor().min;
    let bottom = ui.max_rect().bottom();

    // egui lays out in one pass, so rows go below the previous frame's row heights, and
    // cards stretch to the tallest card's natural height from the previous frame
    // (repainting once when either changes).
    let layout_id = ui.next_auto_id().with("card_rows");
    let previous: RowLayout = ui
        .data(|d| d.get_temp::<RowLayout>(layout_id))
        .filter(|l| l.starts == starts)
        .unwrap_or_default();
    let mut row_tops = Vec::with_capacity(starts.len());
    let mut y = top_left.y;
    for r in 0..starts.len() {
        row_tops.push(y);
        let gap = if stretch_cards {
            CARD_GAP
        } else {
            COLUMN_ROW_GAP
        };
        y += previous.heights.get(r).copied().unwrap_or(0.0) + gap;
    }

    let mut x = top_left.x;
    let mut columns: [Ui; N] = std::array::from_fn(|i| {
        let r = row_of(i);
        let range = row_range(r);
        if i == range.start {
            x = top_left.x;
        }
        let row = &weights[range.clone()];
        let usable = (full_width - spacing * (row.len() as f32 - 1.0)).max(0.0);
        let w = usable * weights[i] / row.iter().sum::<f32>();
        let rect = egui::Rect::from_min_max(pos2(x, row_tops[r]), pos2(x + w, bottom));
        x += w + spacing;
        let mut column = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down_justified(egui::Align::LEFT)),
        );
        column.set_width(w);
        column
    });
    if stretch_cards {
        for (i, column) in columns.iter().enumerate() {
            if let Some(&height) = previous.natural.get(row_of(i)) {
                ui.data_mut(|d| d.insert_temp(card_stretch_id(column), height));
            }
        }
    }
    let result = add_contents(&mut columns);

    let rows = 0..starts.len();
    let heights: Vec<f32> = rows
        .clone()
        .map(|r| {
            columns[row_range(r)]
                .iter()
                .map(|c| c.min_size().y)
                .fold(0.0, f32::max)
        })
        .collect();
    let natural: Vec<f32> = if stretch_cards {
        rows.map(|r| {
            columns[row_range(r)]
                .iter()
                .map(|c| {
                    ui.data_mut(|d| d.remove_temp::<f32>(card_natural_id(c)))
                        .unwrap_or(c.min_size().y)
                })
                .fold(0.0, f32::max)
        })
        .collect()
    } else {
        Vec::new()
    };
    let changed = |a: &[f32], b: &[f32]| {
        a.len() != b.len() || a.iter().zip(b).any(|(a, b)| (a - b).abs() > 0.5)
    };
    if changed(&heights, &previous.heights) || changed(&natural, &previous.natural) {
        ui.ctx().request_repaint();
    }
    let total = row_tops.last().copied().unwrap_or(top_left.y) - top_left.y
        + heights.last().copied().unwrap_or(0.0);
    ui.data_mut(|d| {
        d.insert_temp(
            layout_id,
            RowLayout {
                starts,
                heights,
                natural,
            },
        )
    });
    ui.advance_cursor_after_rect(egui::Rect::from_min_size(
        top_left,
        egui::vec2(full_width, total),
    ));
    result
}

/// The height a card in this column of a [`weighted_columns`] row stretches to.
fn card_stretch_id(column: &Ui) -> egui::Id {
    column.unique_id().with("card_stretch")
}

/// A card's height before stretching, reported to its [`weighted_columns`] row.
fn card_natural_id(column: &Ui) -> egui::Id {
    column.unique_id().with("card_natural")
}

/// A [`card`] with extra widgets (e.g. a dropdown) set into the top border right after
/// the title: `┌─ Title [1h ▾] ───┐`. Both closures get `state`, so they can share
/// mutable data such as the app. Returns the card's rect (its border).
pub fn card_with_header<T: ?Sized>(
    ui: &mut Ui,
    title: &str,
    state: &mut T,
    add_header: impl FnOnce(&mut Ui, &mut T),
    add_contents: impl FnOnce(&mut Ui, &mut T),
) -> egui::Rect {
    let font = FontId::monospace(theme::CARD_TITLE_SIZE);
    let galley = ui
        .painter()
        .layout_no_wrap(format!(" {title} "), font, theme::ACCENT);
    let title_height = galley.size().y;

    // Room above the border for the top half of the title.
    ui.add_space(title_height / 2.0);
    let margin = Margin {
        left: 10,
        right: 10,
        top: 11,
        bottom: 6,
    };
    let stroke = Stroke::new(1.0_f32, theme::BORDER_HI);
    // Everything but the contents: the title's top half, the margins and the border.
    let overhead = title_height / 2.0 + margin.sum().y + 2.0 * stroke.width;
    let stretch_id = card_stretch_id(ui);
    let stretch_to: Option<f32> = ui.data_mut(|d| d.remove_temp(stretch_id));
    let mut natural = 0.0;
    let rect = egui::Frame::new()
        .fill(theme::BG)
        .stroke(stroke)
        .corner_radius(3)
        .inner_margin(margin)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Before the contents: the min height counts from the cursor.
            if let Some(height) = stretch_to {
                ui.set_min_height(height - overhead);
            }
            natural = ui
                .scope(|ui| add_contents(ui, state))
                .response
                .rect
                .height();
        })
        .response
        .rect;
    ui.data_mut(|d| d.insert_temp(card_natural_id(ui), natural + overhead));

    // Cut the border behind the title, then draw it.
    let pos = pos2(rect.left() + 10.0, rect.top() - title_height / 2.0);
    let title_rect = egui::Rect::from_min_size(pos, galley.size());
    let painter = ui.painter();
    painter.rect_filled(title_rect, 0.0, theme::BG);
    painter.galley(pos, galley, theme::ACCENT);

    // Header widgets centered on the border line, after the title. A child UI, so they
    // take no space in the parent layout.
    let height = ui.spacing().interact_size.y;
    let header_rect = egui::Rect::from_min_max(
        pos2(title_rect.right(), rect.top() - height / 2.0),
        pos2(rect.right() - 10.0, rect.top() + height / 2.0),
    );
    // Reserve the border cut below the widgets; sized once they are laid out.
    let cut = ui.painter().add(egui::Shape::Noop);
    let mut header = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(header_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    add_header(&mut header, state);
    let used = header.min_rect();
    if used.width() > 0.0 {
        // A space's width of border cut after the widgets, like the title's padding.
        let gap = title_rect.width() / (title.chars().count() + 2) as f32;
        ui.painter().set(
            cut,
            egui::Shape::rect_filled(
                egui::Rect::from_min_max(
                    pos2(used.left(), rect.top() - 1.0),
                    pos2(used.right() + gap, rect.top() + 1.0),
                ),
                0.0,
                theme::BG,
            ),
        );
    }
    rect
}

/// A heading over a group of rows inside a card (e.g. a [`kv_columns`] column), in white.
pub fn subheader(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).color(theme::TEXT_BRIGHT));
}

/// Accent-colored heading: sections that aren't cards (side panels, help) and table
/// column headers.
pub fn section_title(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).color(theme::ACCENT));
}

/// Show `window` centered and non-collapsible, closing with its X button or Esc.
/// Returns false once closed. Size and resizability stay with the caller's `window`.
pub fn modal_window(
    ctx: &egui::Context,
    window: egui::Window<'_>,
    add_contents: impl FnOnce(&mut Ui),
) -> bool {
    let mut open = true;
    window
        .open(&mut open)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, add_contents);
    open && !ctx.input(|i| i.key_pressed(egui::Key::Escape))
}

/// `value` formatted with `f`, or an em dash when there is none.
pub fn or_dash<T>(value: Option<T>, f: impl FnOnce(T) -> String) -> String {
    value.map_or_else(|| "—".to_string(), f)
}

/// Read-only (but selectable/copyable) JSON with a link icon after each block hash in
/// `links`. Returns the hash whose icon was clicked.
pub fn json_view(ui: &mut Ui, text: &str, links: &[HashLink]) -> Option<String> {
    let output = TextEdit::multiline(&mut &*text)
        .code_editor()
        .desired_width(f32::INFINITY)
        .show(ui);

    let clip = ui.clip_rect();
    let size = ui.text_style_height(&egui::TextStyle::Monospace);
    let mut clicked = None;
    for link in links {
        let line = output
            .galley
            .pos_from_cursor(CCursor::new(link.line_end_char))
            .translate(output.galley_pos.to_vec2());
        let rect = egui::Rect::from_min_size(
            pos2(line.right() + 6.0, line.center().y - size / 2.0),
            egui::vec2(size, size),
        );
        if !clip.intersects(rect) {
            continue;
        }
        let icon = ui
            .put(
                rect,
                Button::new(RichText::new("🔍").size(size * 0.8).color(theme::ACCENT)).frame(false),
            )
            .on_hover_text("Open block")
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if icon.clicked() {
            clicked = Some(link.hash.clone());
        }
    }
    clicked
}

/// A two-column label/value grid. Fill it with [`kv`].
pub fn kv_grid(ui: &mut Ui, id: &str, add_rows: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([16.0, KV_ROW_GAP])
        .min_row_height(theme::KV_ROW_HEIGHT)
        .show(ui, |ui| {
            ui.data_mut(|d| d.insert_temp(kv_row_id(ui), KvRow::default()));
            add_rows(ui)
        });
}

/// Vertical gap between [`kv_grid`] rows, covered by the rows' stripes.
const KV_ROW_GAP: f32 = 1.0;

/// The [`kv_grid`] being filled: the next row's index (for striping) and where the last
/// row's stripe ended, so the next one starts exactly there.
#[derive(Clone, Copy, Default)]
struct KvRow {
    index: usize,
    bottom: Option<f32>,
}

/// The next row's index in the [`kv_grid`] being filled, for striping.
fn kv_row_id(grid: &Ui) -> egui::Id {
    grid.id().with("kv_row")
}

/// A dim field label with a trailing colon, e.g. `Network:`.
pub fn field_label(ui: &mut Ui, label: &str) -> egui::Response {
    ui.label(RichText::new(format!("{label}:")).color(theme::LABEL))
}

/// One row of a [`kv_grid`]: the label on the left, the value against the right edge.
pub fn kv(ui: &mut Ui, label: &str, value: impl Into<WidgetText>) {
    kv_with(ui, label, |ui| {
        ui.label(value);
    });
}

/// A [`kv`] row with a custom value. Widgets added by `add_value` run right to left, so
/// the first one ends up rightmost.
pub fn kv_with(ui: &mut Ui, label: &str, add_value: impl FnOnce(&mut Ui)) {
    // Reserved behind the row, sized once it is laid out.
    let stripe = ui.painter().add(egui::Shape::Noop);
    let label_rect = field_label(ui, label).rect;
    // The grid's last column spans the rest of the row, so this pins values to the right
    // edge. Not in tooltips, which would stretch to their maximum width.
    let tooltip = ui.layer_id().order == egui::Order::Tooltip;
    let value_rect = if tooltip {
        ui.horizontal(add_value).response.rect
    } else {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add_value)
            .response
            .rect
    };
    ui.end_row();

    // Every other row shaded across the full width, like the striped tables, and the row
    // under the pointer highlighted. egui's own grid stripes stop at the value's width,
    // since the value cell is right-aligned.
    // Cells are centered vertically in the row, so center the stripe on them and grow it
    // to the row's height. Rows tile: each stripe starts where the previous one ended.
    let y = label_rect.union(value_rect).y_range();
    let half = (y.span().max(theme::KV_ROW_HEIGHT) + KV_ROW_GAP) / 2.0;
    let bottom = y.center() + half;
    let previous = ui.data_mut(|d| {
        let state = d.get_temp_mut_or_default::<KvRow>(kv_row_id(ui));
        let previous = *state;
        *state = KvRow {
            index: previous.index + 1,
            bottom: Some(bottom),
        };
        previous
    });
    if tooltip {
        return;
    }
    let top = previous.bottom.unwrap_or(y.center() - half);
    let x = ui.max_rect().x_range().expand(2.0);
    let rect = egui::Rect::from_x_y_ranges(x, top..=bottom);
    // Half-open, so a pointer on the line between two rows hovers only the lower one.
    let hovered = ui.rect_contains_pointer(rect)
        && ui.ctx().pointer_hover_pos().is_some_and(|p| p.y < bottom);
    let row = previous.index;
    let fill = if hovered {
        theme::ROW_HOVER
    } else if row.is_multiple_of(2) {
        ui.visuals().faint_bg_color
    } else {
        return;
    };
    ui.painter()
        .set(stripe, egui::Shape::rect_filled(rect, 2.0, fill));
}

/// `text` in full if it fits the available width, otherwise shortened in the middle
/// (`kaspa:qzv6...3gujgy`) to fit, with the full text on hover. Re-fit every frame, so it
/// follows window resizes both ways.
pub fn fit_label(ui: &mut Ui, text: &str) -> egui::Response {
    let shown = fit_text(ui, text, ui.available_width());
    let shortened = shown != text;
    let response = ui.add(egui::Label::new(shown).wrap_mode(egui::TextWrapMode::Extend));
    if shortened {
        response.on_hover_text(text)
    } else {
        response
    }
}

/// `text` shortened in the middle to fit `width` in the body font.
fn fit_text(ui: &Ui, text: &str, width: f32) -> String {
    let font = egui::TextStyle::Body.resolve(ui.style());
    // All fonts are monospace, so one glyph's width sizes the whole string.
    let glyph = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(1.0);
    shorten_middle(text, (width / glyph).floor().max(0.0) as usize)
}

/// How long the copy icon shows a check mark after copying.
const COPIED_FOR: f64 = 1.5;

/// What a [`linked_value`] is, which decides its menu.
#[derive(Clone, Copy)]
enum LinkKind {
    Address,
    Block,
}

/// A Kaspa address: fitted like [`fit_label`], with a copy icon, a hover highlight and a
/// click menu to open it in a block explorer. Use this for every address shown.
pub fn address(ui: &mut Ui, addr: &str) {
    linked_value(ui, addr, LinkKind::Address, false, true);
}

/// [`address`] without its label chip, for a table that shows the label in its own
/// column (see [`label_cell`]).
pub fn address_bare(ui: &mut Ui, addr: &str) {
    linked_value(ui, addr, LinkKind::Address, false, false);
}

/// The label chip of `addr` on its own (nothing when it has none): a click edits the
/// user's label in place, as on the chip before an [`address`].
pub fn label_cell(ui: &mut Ui, addr: &str) {
    let id = ui.id().with(("label_cell", addr));
    let label = labels(ui.ctx()).and_then(|book| book.get(addr).cloned());
    label_slot(ui, id, addr, label.as_ref());
}

/// A block hash, like [`address`] but a click opens the Block Info window for it (see
/// [`request_block`]) instead of a menu: hashes such as DAG tips refresh too quickly for a
/// menu to stay open. Block Info has the explorer links. `selected` keeps it highlighted,
/// e.g. for the block on show. Returns true when clicked. Use this for every block hash
/// shown.
pub fn block_hash(ui: &mut Ui, hash: &str, selected: bool) -> bool {
    linked_value(ui, hash, LinkKind::Block, selected, false)
}

/// `chip`: show a known address's label chip (and the inline editor) before it.
fn linked_value(ui: &mut Ui, value: &str, kind: LinkKind, selected: bool, chip: bool) -> bool {
    if !ui.layout().is_horizontal() {
        return ui
            .horizontal(|ui| linked_value(ui, value, kind, selected, chip))
            .inner;
    }
    let id = ui.id().with(("linked_value", value));
    let copy_hint = match kind {
        LinkKind::Address => "Copy address",
        LinkKind::Block => "Copy hash",
    };

    // A known address shows its label first (see [`set_labels`]); a click on the chip,
    // or the right-click menu, edits the user's own label in place.
    let rtl = ui.layout().prefer_right_to_left();
    let book = match kind {
        LinkKind::Address => labels(ui.ctx()),
        LinkKind::Block => None,
    };
    let label = book
        .as_ref()
        .filter(|_| chip)
        .and_then(|book| book.get(value).cloned());
    let user_label = book
        .as_ref()
        .and_then(|book| book.user_labels().get(value).cloned());
    if !rtl && chip {
        label_slot(ui, id, value, label.as_ref());
    }

    // Leave room for the label's padding and the copy icon.
    let padding = 2.0 * ui.spacing().button_padding.x;
    let room = ui.available_width() - padding - COPY_ICON_SIZE - COPY_ICON_GAP;
    let shown = fit_text(ui, value, room);

    // In a right-to-left row (a right-aligned value) the first widget lands rightmost, so
    // add the icon first to keep it after the value.
    if rtl {
        copy_button(ui, id, value, copy_hint);
    }

    // Same hover and selected look as list items; addresses also stay selected while a
    // menu of theirs is open.
    let menu_id = id.with("menu");
    let context_id = id.with("context");
    let menu_open =
        egui::Popup::is_id_open(ui.ctx(), menu_id) || egui::Popup::is_id_open(ui.ctx(), context_id);
    let response = icon_gap(ui, rtl, |ui| {
        ui.selectable_label(selected || menu_open, shown.as_str())
    })
    .on_hover_cursor(egui::CursorIcon::PointingHand);
    let hint = match kind {
        LinkKind::Address => Some("Click for address info, right-click to label"),
        LinkKind::Block => Some("Click for block info"),
    };
    let response = match (shown != value, hint) {
        (true, Some(hint)) => response.on_hover_text(format!("{value}\n{hint}")),
        (true, None) => response.on_hover_text(value),
        (false, Some(hint)) => response.on_hover_text(hint),
        (false, None) => response,
    };

    let mut get_block = false;
    match kind {
        LinkKind::Address => {
            egui::Popup::menu(&response).id(menu_id).show(|ui| {
                if ui.button("Address info").clicked() {
                    request_address(ui.ctx(), value);
                }
                for (name, url) in [
                    ("Open in Kaspa Explorer", explorer_address_url(value)),
                    ("Open in Kaspa Stream", kaspa_stream_address_url(value)),
                ] {
                    if ui.button(name).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                }
            });
            // Right click: the user's own label (over any public one).
            egui::Popup::context_menu(&response)
                .id(context_id)
                .show(|ui| match user_label.as_deref() {
                    Some(current) => {
                        if ui.button("Edit Address Label").clicked() {
                            start_label_edit(ui.ctx(), id, current);
                        }
                        if ui.button("Remove Address Label").clicked() {
                            request_label(ui.ctx(), value, None);
                        }
                    }
                    None => {
                        if ui.button("Add Address Label").clicked() {
                            start_label_edit(ui.ctx(), id, "");
                        }
                    }
                });
        }
        LinkKind::Block => {
            if response.clicked() {
                request_block(ui.ctx(), value);
                get_block = true;
            }
        }
    }

    if !rtl {
        icon_gap(ui, true, |ui| copy_button(ui, id, value, copy_hint));
    }
    if rtl && chip {
        label_slot(ui, id, value, label.as_ref());
    }
    get_block
}

/// The place of an address's label chip: the inline editor while this widget is editing
/// the label (see [`start_label_edit`]), else the chip for a known address, else nothing.
fn label_slot(ui: &mut Ui, widget: egui::Id, address: &str, label: Option<&Label>) {
    if let Some(mut edit) = label_edit(ui.ctx()).filter(|e| e.widget == widget) {
        let response = ui.add(
            TextEdit::singleline(&mut edit.draft)
                .hint_text("label")
                .font(FontId::monospace(theme::SMALL_FONT_SIZE))
                .desired_width(LABEL_EDIT_WIDTH)
                .margin(Margin::symmetric(4, 1)),
        );
        if edit.fresh {
            response.request_focus();
            edit.fresh = false;
        }
        if response.lost_focus() {
            // Enter saves (an empty name removes the user's label); Esc or a click
            // elsewhere abandons the edit.
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                request_label(
                    ui.ctx(),
                    address,
                    Some(edit.draft.trim().to_string()).filter(|d| !d.is_empty()),
                );
            }
            clear_label_edit(ui.ctx());
        } else if !response.has_focus() {
            // Never got the focus (the widget was off screen when the edit started).
            clear_label_edit(ui.ctx());
        } else {
            set_label_edit(ui.ctx(), Some(edit));
        }
        return;
    }
    if let Some(label) = label {
        label_chip(ui, widget, label);
    }
}

/// A known address's label, as a small accent chip before the address. A click edits
/// the user's label in place, starting from the name shown.
fn label_chip(ui: &mut Ui, widget: egui::Id, label: &Label) {
    let response = egui::Frame::new()
        .fill(theme::ACCENT_DIM)
        .corner_radius(3)
        .inner_margin(Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(&label.name)
                        .color(theme::ACCENT_BRIGHT)
                        .size(theme::SMALL_FONT_SIZE),
                )
                .sense(egui::Sense::click()),
            )
        })
        .inner
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!(
            "{}\nClick to edit your label",
            match label.source {
                LabelSource::User => "Your label".to_string(),
                source => format!("Label from {}", source.label()),
            }
        ));
    if response.clicked() {
        start_label_edit(ui.ctx(), widget, &label.name);
    }
}

/// Width of the inline label editor.
const LABEL_EDIT_WIDTH: f32 = 140.0;

/// The one inline label edit in progress, if any: which [`address`] widget shows it.
#[derive(Clone)]
struct LabelEdit {
    widget: egui::Id,
    draft: String,
    /// Not yet focused (set on the frame the edit starts).
    fresh: bool,
}

impl Default for LabelEdit {
    fn default() -> Self {
        Self {
            widget: egui::Id::NULL,
            draft: String::new(),
            fresh: false,
        }
    }
}

fn label_edit_id() -> egui::Id {
    egui::Id::new("address_label_edit")
}

fn label_edit(ctx: &egui::Context) -> Option<LabelEdit> {
    ctx.data(|d| d.get_temp(label_edit_id()))
}

fn set_label_edit(ctx: &egui::Context, edit: Option<LabelEdit>) {
    ctx.data_mut(|d| match edit {
        Some(edit) => d.insert_temp(label_edit_id(), edit),
        None => {
            d.remove_temp::<LabelEdit>(label_edit_id());
        }
    });
}

fn clear_label_edit(ctx: &egui::Context) {
    set_label_edit(ctx, None);
}

/// Turn the label chip of the [`address`] widget `widget` into an editor, starting
/// from `initial`. Only one edit runs at a time.
fn start_label_edit(ctx: &egui::Context, widget: egui::Id, initial: &str) {
    set_label_edit(
        ctx,
        Some(LabelEdit {
            widget,
            draft: initial.to_string(),
            fresh: true,
        }),
    );
}

fn label_request_id() -> egui::Id {
    egui::Id::new("address_label_requests")
}

/// Ask to set (or with `None`, remove) the user's label for `address`, from anywhere.
/// The GUI frame loop picks it up with [`take_label_requests`] and sends `SetLabel`.
pub fn request_label(ctx: &egui::Context, address: &str, name: Option<String>) {
    let request = (address.trim().to_string(), name);
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Vec<(String, Option<String>)>>(label_request_id())
            .push(request);
    });
}

/// The label changes requested this frame.
pub fn take_label_requests(ctx: &egui::Context) -> Vec<(String, Option<String>)> {
    ctx.data_mut(|d| d.remove_temp(label_request_id()))
        .unwrap_or_default()
}

fn address_request_id() -> egui::Id {
    egui::Id::new("address_lookup_request")
}

/// Ask for the Address Info window for `address`, from any tab. The GUI frame loop picks
/// it up with [`take_address_request`] and sends the lookup.
pub fn request_address(ctx: &egui::Context, address: &str) {
    ctx.data_mut(|d| d.insert_temp(address_request_id(), address.trim().to_string()));
}

/// The address requested this frame, if any.
pub fn take_address_request(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.remove_temp::<String>(address_request_id()))
}

fn labels_id() -> egui::Id {
    egui::Id::new("address_labels")
}

/// Make the app's labels available to [`address`] widgets anywhere; set once per frame.
pub fn set_labels(ctx: &egui::Context, book: Arc<LabelBook>) {
    ctx.data_mut(|d| d.insert_temp(labels_id(), book));
}

fn labels(ctx: &egui::Context) -> Option<Arc<LabelBook>> {
    ctx.data(|d| d.get_temp(labels_id()))
}

/// `value` as plain text, fitted like [`fit_label`], followed by a copy icon.
pub fn copy_value(ui: &mut Ui, value: &str, hint: &str) {
    if !ui.layout().is_horizontal() {
        ui.horizontal(|ui| copy_value(ui, value, hint));
        return;
    }
    let id = ui.id().with(("copy_value", value));
    // Right to left (a right-aligned value): the icon first so it lands after the value.
    let rtl = ui.layout().prefer_right_to_left();
    if rtl {
        copy_button(ui, id, value, hint);
    }
    let room = ui.available_width()
        - if rtl {
            0.0
        } else {
            COPY_ICON_SIZE + COPY_ICON_GAP
        };
    let shown = fit_text(ui, value, room);
    let label = icon_gap(ui, rtl, |ui| {
        ui.add(egui::Label::new(shown.as_str()).wrap_mode(egui::TextWrapMode::Extend))
    });
    if shown != value {
        label.on_hover_text(value);
    }
    if !rtl {
        icon_gap(ui, true, |ui| copy_button(ui, id, value, hint));
    }
}

/// Adds `add` [`COPY_ICON_GAP`] after the previous widget when `apply` (the value and its
/// copy icon, whichever comes second in the row), else with the usual item spacing.
fn icon_gap<R>(ui: &mut Ui, apply: bool, add: impl FnOnce(&mut Ui) -> R) -> R {
    if !apply {
        return add(ui);
    }
    let spacing = ui.spacing().item_spacing.x;
    ui.spacing_mut().item_spacing.x = COPY_ICON_GAP;
    let result = add(ui);
    ui.spacing_mut().item_spacing.x = spacing;
    result
}

/// The copy icon for `value`: copies it on click and shows a check mark for a moment.
/// `id` keeps that state, `hint` is the hover text.
fn copy_button(ui: &mut Ui, id: egui::Id, value: &str, hint: &str) {
    let copied_at: Option<f64> = ui.ctx().data(|d| d.get_temp(id));
    let now = ui.input(|i| i.time);
    let just_copied = copied_at.is_some_and(|t| now - t < COPIED_FOR);
    let copy = copy_icon(ui, just_copied).on_hover_text(if just_copied { "Copied" } else { hint });
    if copy.clicked() {
        ui.ctx().copy_text(value.to_string());
        ui.ctx().data_mut(|d| d.insert_temp(id, now));
    }
    if just_copied {
        // Revert to the copy icon once the check mark has shown long enough.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(COPIED_FOR));
    }
}

fn block_request_id() -> egui::Id {
    egui::Id::new("block_lookup_request")
}

/// Ask for the Block Info window for `hash`, from any tab. The GUI frame loop picks it up
/// with [`take_block_request`] and sends the lookup.
pub fn request_block(ctx: &egui::Context, hash: &str) {
    ctx.data_mut(|d| d.insert_temp(block_request_id(), hash.to_string()));
}

/// The block hash requested this frame, if any.
pub fn take_block_request(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.remove_temp::<String>(block_request_id()))
}

fn testnet_id() -> egui::Id {
    egui::Id::new("network_is_testnet")
}

/// Record whether the connected network is a testnet, for explorer links to blocks (whose
/// hashes, unlike addresses, don't say). Set once per frame by the GUI; read with
/// [`is_testnet`].
pub fn set_testnet(ctx: &egui::Context, testnet: bool) {
    ctx.data_mut(|d| d.insert_temp(testnet_id(), testnet));
}

pub fn is_testnet(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp(testnet_id())).unwrap_or(false)
}

/// Side of the square copy icon, about the height of a capital letter.
const COPY_ICON_SIZE: f32 = 11.0;
/// Gap between a value and its copy icon, a little tighter than the usual item spacing.
const COPY_ICON_GAP: f32 = 5.0;

/// The usual copy icon, two overlapping rounded squares, or a check mark once copied.
/// Brightens on hover.
fn copy_icon(ui: &mut Ui, copied: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(COPY_ICON_SIZE, COPY_ICON_SIZE),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let color = if copied {
        theme::OK
    } else if response.hovered() {
        theme::ACCENT_BRIGHT
    } else {
        theme::TEXT_DIM
    };
    let stroke = Stroke::new(1.2_f32, color);
    let painter = ui.painter();
    let r = rect.shrink(0.5);

    if copied {
        painter.line(
            vec![
                pos2(r.left() + 1.0, r.center().y),
                pos2(r.left() + r.width() * 0.4, r.bottom() - 1.5),
                pos2(r.right() - 0.5, r.top() + 1.5),
            ],
            stroke,
        );
        return response;
    }

    // Front sheet at the bottom right, back sheet peeking out at the top left.
    let offset = (r.width() * 0.3).round();
    let side = r.width() - offset;
    let front =
        egui::Rect::from_min_size(r.min + egui::vec2(offset, offset), egui::vec2(side, side));
    painter.rect_stroke(front, 1.5, stroke, egui::StrokeKind::Middle);
    // Only the back sheet's edges that the front one doesn't cover.
    painter.line(
        vec![
            pos2(r.left() + offset - 1.5, r.top() + side),
            pos2(r.left(), r.top() + side),
            pos2(r.left(), r.top()),
            pos2(r.left() + side, r.top()),
            pos2(r.left() + side, r.top() + offset - 1.5),
        ],
        stroke,
    );
    response
}

/// Greyed-out placeholder text, e.g. "Waiting for data…".
pub fn placeholder(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak());
}

pub fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

/// The main action of a form: filled with the accent color.
pub fn primary_button(text: &str) -> Button<'static> {
    Button::new(RichText::new(text).color(theme::BG_DEEP)).fill(theme::ACCENT)
}

/// A thin vertical divider between status bar segments.
pub fn divider(ui: &mut Ui) {
    ui.label(RichText::new("│").color(theme::BORDER_HI));
}

/// A colored status-bar indicator, e.g. `● Node synced`, with a details grid on hover.
/// Fill the grid with [`kv`] rows.
pub fn status_chip(
    ui: &mut Ui,
    id: &str,
    text: &str,
    color: egui::Color32,
    details: impl FnOnce(&mut Ui),
) {
    ui.label(RichText::new(text).color(color))
        .on_hover_ui(|ui| kv_grid(ui, id, details));
}

/// Placeholder text for data that needs a direct node (a URL), not the resolver.
pub fn direct_node_placeholder<'a>(app: &App, waiting: &'a str) -> &'a str {
    match app.connection {
        _ if app.connection.is_direct() => waiting,
        ActiveConnection::Resolver => "Disabled when using the public resolver",
        _ => "Not connected",
    }
}
