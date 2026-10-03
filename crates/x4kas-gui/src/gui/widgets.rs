//! Small building blocks shared by the tab views.

use eframe::egui::{
    self, Button, FontId, Margin, RichText, Stroke, TextEdit, Ui, WidgetText, pos2, text::CCursor,
};

use super::theme;
use x4kas_core::app::{ActiveConnection, App};
use x4kas_core::format::{explorer_address_url, kaspa_stream_address_url, shorten_middle};
use x4kas_core::rpc::hash_links::HashLink;

/// Vertical space between stacked cards.
pub const CARD_GAP: f32 = 4.0;

/// A bordered pane with its title set into the top border, like a TUI block:
/// `┌─ Title ───────┐`.
pub fn card(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    card_with_header(ui, title, &mut (), |_, _| {}, |ui, _| add_contents(ui));
}

/// A [`card`] with extra widgets (e.g. a dropdown) set into the top border right after
/// the title: `┌─ Title [1h ▾] ───┐`. Both closures get `state`, so they can share
/// mutable data such as the app.
pub fn card_with_header<T: ?Sized>(
    ui: &mut Ui,
    title: &str,
    state: &mut T,
    add_header: impl FnOnce(&mut Ui, &mut T),
    add_contents: impl FnOnce(&mut Ui, &mut T),
) {
    let font = FontId::monospace(theme::FONT_SIZE);
    let galley = ui
        .painter()
        .layout_no_wrap(format!(" {title} "), font, theme::ACCENT);
    let title_height = galley.size().y;

    // Room above the border for the top half of the title.
    ui.add_space(title_height / 2.0);
    let rect = egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0_f32, theme::BORDER_HI))
        .corner_radius(3)
        .inner_margin(Margin {
            left: 10,
            right: 10,
            top: 11,
            bottom: 6,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui, state);
        })
        .response
        .rect;

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
        .spacing([16.0, 1.0])
        .min_row_height(theme::ROW_HEIGHT)
        .show(ui, add_rows);
}

/// A dim field label with a trailing colon, e.g. `Network:`.
pub fn field_label(ui: &mut Ui, label: &str) -> egui::Response {
    ui.label(RichText::new(format!("{label}:")).weak())
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
    field_label(ui, label);
    // The grid's last column spans the rest of the row, so this pins values to the right
    // edge. Not in tooltips, which would stretch to their maximum width.
    if ui.layer_id().order == egui::Order::Tooltip {
        ui.horizontal(add_value);
    } else {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add_value);
    }
    ui.end_row();
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
    linked_value(ui, addr, LinkKind::Address, false);
}

/// A block hash, like [`address`] but a click opens the Block Info window for it (see
/// [`request_block`]) instead of a menu: hashes such as DAG tips refresh too quickly for a
/// menu to stay open. Block Info has the explorer links. `selected` keeps it highlighted,
/// e.g. for the block on show. Returns true when clicked. Use this for every block hash
/// shown.
pub fn block_hash(ui: &mut Ui, hash: &str, selected: bool) -> bool {
    linked_value(ui, hash, LinkKind::Block, selected)
}

fn linked_value(ui: &mut Ui, value: &str, kind: LinkKind, selected: bool) -> bool {
    if !ui.layout().is_horizontal() {
        return ui
            .horizontal(|ui| linked_value(ui, value, kind, selected))
            .inner;
    }
    let id = ui.id().with(("linked_value", value));
    let copy_hint = match kind {
        LinkKind::Address => "Copy address",
        LinkKind::Block => "Copy hash",
    };

    // Leave room for the label's padding and the copy icon.
    let padding = 2.0 * ui.spacing().button_padding.x;
    let room = ui.available_width() - padding - COPY_ICON_SIZE - ui.spacing().item_spacing.x;
    let shown = fit_text(ui, value, room);

    // In a right-to-left row (a right-aligned value) the first widget lands rightmost, so
    // add the icon first to keep it after the value.
    let rtl = ui.layout().prefer_right_to_left();
    if rtl {
        copy_button(ui, id, value, copy_hint);
    }

    // Same hover and selected look as list items; addresses also stay selected while their
    // menu is open.
    let menu_id = id.with("menu");
    let menu_open = egui::Popup::is_id_open(ui.ctx(), menu_id);
    let response = ui
        .selectable_label(selected || menu_open, shown.as_str())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let hint = match kind {
        LinkKind::Address => None,
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
                for (name, url) in [
                    ("Open in Kaspa Explorer", explorer_address_url(value)),
                    ("Open in Kaspa Stream", kaspa_stream_address_url(value)),
                ] {
                    if ui.button(name).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
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
        copy_button(ui, id, value, copy_hint);
    }
    get_block
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
            COPY_ICON_SIZE + ui.spacing().item_spacing.x
        };
    let shown = fit_text(ui, value, room);
    let label = ui.add(egui::Label::new(shown.as_str()).wrap_mode(egui::TextWrapMode::Extend));
    if shown != value {
        label.on_hover_text(value);
    }
    if !rtl {
        copy_button(ui, id, value, hint);
    }
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
