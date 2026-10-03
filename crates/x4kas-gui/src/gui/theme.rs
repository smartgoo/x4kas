//! Palette and global egui style: a dark "phosphor terminal" look in Kaspa teal.
//!
//! Views pick colors from the constants here instead of hard-coding them, and
//! [`apply`] installs the matching fonts and widget visuals once at startup.

use eframe::egui::{
    self, Color32, CornerRadius, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke,
    TextStyle, Theme, Visuals, vec2,
};

use x4kas_core::app::ConnectionStatus;

// ── Surfaces ──
/// Main background (central panel, cards).
pub const BG: Color32 = Color32::from_rgb(0x0d, 0x12, 0x11);
/// Darkest background: text fields, plots, the terminal.
pub const BG_DEEP: Color32 = Color32::from_rgb(0x08, 0x0b, 0x0b);
/// Raised surfaces: top/status bars, windows.
pub const SURFACE: Color32 = Color32::from_rgb(0x14, 0x1b, 0x1a);
/// Button and widget background.
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x1c, 0x26, 0x25);
const SURFACE_HOVER: Color32 = Color32::from_rgb(0x25, 0x33, 0x31);
pub const BORDER: Color32 = Color32::from_rgb(0x27, 0x34, 0x33);
pub const BORDER_HI: Color32 = Color32::from_rgb(0x3b, 0x4d, 0x4b);

// ── Text ──
pub const TEXT: Color32 = Color32::from_rgb(0xcd, 0xd8, 0xd6);
/// Labels and secondary text (`.weak()`). Kept above 6:1 contrast on `SURFACE`.
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8a, 0x9c, 0x99);
pub const TEXT_BRIGHT: Color32 = Color32::from_rgb(0xef, 0xfa, 0xf8);

// ── Accents and status ──
/// Kaspa teal.
pub const ACCENT: Color32 = Color32::from_rgb(0x70, 0xc7, 0xba);
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(0x49, 0xea, 0xcb);
/// Selection background (selected list items, toggles, text selection).
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x1d, 0x4a, 0x43);
pub const OK: Color32 = Color32::from_rgb(0x5f, 0xd3, 0x8d);
pub const WARN: Color32 = Color32::from_rgb(0xf0, 0xb5, 0x4a);
pub const ERROR: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);

// ── DAG visualizer ──
pub const DAG_BLOCK: Color32 = TEXT_BRIGHT;
/// Blocks no other block references yet.
pub const DAG_TIP: Color32 = ACCENT_BRIGHT;
pub const DAG_HOVER: Color32 = OK;
/// Parents of the hovered block, and the edges to them.
pub const DAG_PARENT: Color32 = WARN;
pub const DAG_EDGE: Color32 = TEXT_DIM;

pub const FONT_SIZE: f32 = 12.0;
pub const SMALL_FONT_SIZE: f32 = 10.0;
/// Minimum height of a read-only grid row (egui defaults to the button height).
pub const ROW_HEIGHT: f32 = 14.0;

/// Install fonts and visuals. The app is always dark, whatever the system theme.
pub fn apply(ctx: &egui::Context) {
    // Monospace everywhere (egui's bundled Hack, with its emoji/symbol fallbacks).
    let mut fonts = FontDefinitions::default();
    let mono = fonts.families[&FontFamily::Monospace].clone();
    fonts.families.insert(FontFamily::Proportional, mono);
    ctx.set_fonts(fonts);

    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, |style| {
        let mono = |size| FontId::new(size, FontFamily::Monospace);
        style.text_styles = [
            (TextStyle::Heading, mono(14.0)),
            (TextStyle::Body, mono(FONT_SIZE)),
            (TextStyle::Monospace, mono(FONT_SIZE)),
            (TextStyle::Button, mono(FONT_SIZE)),
            (TextStyle::Small, mono(SMALL_FONT_SIZE)),
        ]
        .into();

        let spacing = &mut style.spacing;
        spacing.item_spacing = vec2(8.0, 3.0);
        spacing.button_padding = vec2(6.0, 1.0);
        spacing.interact_size.y = 18.0;
        spacing.window_margin = Margin::same(10);
        spacing.menu_margin = Margin::same(6);

        style.visuals = visuals();
    });
}

fn visuals() -> Visuals {
    let radius = CornerRadius::same(2);
    let mut v = Visuals::dark();

    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0_f32, BORDER_HI);
    v.window_corner_radius = CornerRadius::same(3);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 10,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    v.menu_corner_radius = radius;
    v.extreme_bg_color = BG_DEEP;
    v.faint_bg_color = Color32::from_rgb(0x12, 0x19, 0x18); // striped rows
    v.code_bg_color = SURFACE_HI;
    v.hyperlink_color = ACCENT_BRIGHT;
    v.warn_fg_color = WARN;
    v.error_fg_color = ERROR;
    v.weak_text_color = Some(TEXT_DIM);
    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT_BRIGHT);

    let w = &mut v.widgets;
    // Labels, separators, frames.
    w.noninteractive.bg_fill = BG;
    w.noninteractive.weak_bg_fill = BG;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = radius;
    // Buttons, checkboxes, text fields at rest.
    w.inactive.bg_fill = SURFACE_HI;
    w.inactive.weak_bg_fill = SURFACE_HI;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = SURFACE_HOVER;
    w.hovered.weak_bg_fill = SURFACE_HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.hovered.fg_stroke = Stroke::new(1.5_f32, TEXT_BRIGHT);
    w.hovered.corner_radius = radius;
    w.active.bg_fill = ACCENT_DIM;
    w.active.weak_bg_fill = ACCENT_DIM;
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT_BRIGHT);
    w.active.fg_stroke = Stroke::new(1.5_f32, TEXT_BRIGHT);
    w.active.corner_radius = radius;
    w.open.bg_fill = SURFACE_HI;
    w.open.weak_bg_fill = SURFACE_HI;
    w.open.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.open.fg_stroke = Stroke::new(1.0_f32, TEXT_BRIGHT);
    w.open.corner_radius = radius;

    v
}

/// ANSI colors for the integrated terminal, built around the app's palette.
pub fn terminal_palette() -> egui_term::ColorPalette {
    let hex = |c: Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
    let rgb = |s: &str| s.to_string();
    egui_term::ColorPalette {
        foreground: hex(TEXT),
        background: hex(BG_DEEP),
        black: hex(SURFACE_HI),
        red: hex(ERROR),
        green: hex(OK),
        yellow: hex(WARN),
        blue: rgb("#6aa6d6"),
        magenta: rgb("#c28cc8"),
        cyan: hex(ACCENT),
        white: hex(TEXT),
        bright_black: hex(TEXT_DIM),
        bright_red: rgb("#ff8f8f"),
        bright_green: rgb("#8be3ad"),
        bright_yellow: rgb("#f6cd7f"),
        bright_blue: rgb("#8fc1ea"),
        bright_magenta: rgb("#d9a8de"),
        bright_cyan: hex(ACCENT_BRIGHT),
        bright_white: hex(TEXT_BRIGHT),
        bright_foreground: Some(hex(TEXT_BRIGHT)),
        dim_foreground: hex(TEXT_DIM),
        ..Default::default()
    }
}

pub fn connection_status(status: &ConnectionStatus) -> (&'static str, Color32) {
    match status {
        ConnectionStatus::Connected => ("Connected", OK),
        ConnectionStatus::Connecting => ("Connecting…", WARN),
        ConnectionStatus::Disconnected => ("Disconnected", ERROR),
        ConnectionStatus::Error(_) => ("Error", ERROR),
    }
}
