//! The Health card over the Health page's Live view: a small line chart per metric the
//! sampler keeps in `App.metrics` (`x4kas_core::metrics`), plus the GUI's own frame
//! time, over the last 5 minutes, hour or the whole run. Nothing is kept across runs.

use std::time::Duration;

use eframe::egui::{self, Color32, Pos2, RichText, Stroke, Ui, pos2};

use super::theme;
use super::widgets::{card_with_header, kv, kv_grid};
use x4kas_core::app::App;
use x4kas_core::format::{format_clock_ms, format_number, format_size};
use x4kas_core::metrics::{Sample, Series, clock_ms};

/// Height of a chart's plot.
const PLOT_HEIGHT: f32 = 44.0;
/// Charts per row.
const COLUMNS: usize = 4;
/// A frame slower than this misses a 60 Hz display's refresh: drawn as a guide.
const FRAME_BUDGET_MS: f64 = 1000.0 / 60.0;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Window {
    #[default]
    FiveMinutes,
    Hour,
    Run,
}

impl Window {
    const ALL: [Window; 3] = [Window::FiveMinutes, Window::Hour, Window::Run];

    fn label(self) -> &'static str {
        match self {
            Window::FiveMinutes => "5m",
            Window::Hour => "1h",
            Window::Run => "Run",
        }
    }

    fn hover(self) -> &'static str {
        match self {
            Window::FiveMinutes => "The last 5 minutes, every second",
            Window::Hour => "The last hour, every second",
            Window::Run => "Since the app started (a point a minute past the last hour)",
        }
    }

    /// The start of the window, in ms since the epoch.
    fn start_ms(self, now: u64, started: Option<u64>) -> u64 {
        match self {
            Window::FiveMinutes => now.saturating_sub(5 * 60_000),
            Window::Hour => now.saturating_sub(60 * 60_000),
            Window::Run => started.unwrap_or(now).min(now.saturating_sub(60_000)),
        }
    }
}

/// The slowest frame of each second, recorded by the frame loop.
#[derive(Default)]
struct FrameTimes {
    series: Series,
    /// The second being measured (ms since the epoch / 1000) and its slowest frame.
    second: Option<u64>,
    worst_ms: f64,
}

impl FrameTimes {
    fn record(&mut self, took: Duration) {
        let second = clock_ms() / 1000;
        if let Some(s) = self.second
            && s != second
        {
            self.series.push(s * 1000, self.worst_ms);
            self.worst_ms = 0.0;
            // No frames for a while (the window hidden, the app suspended): a gap.
            if second > s + 2 {
                self.series.break_line();
            }
        }
        self.second = Some(second);
        self.worst_ms = self.worst_ms.max(took.as_secs_f64() * 1000.0);
    }
}

#[derive(Default)]
pub struct HealthCharts {
    window: Window,
    frames: FrameTimes,
}

/// One line on a chart.
struct Line<'a> {
    name: &'static str,
    series: &'a Series,
    color: Color32,
}

/// One chart: its lines, how to write a value, the lowest top of its scale (so a quiet
/// metric isn't blown up to full height), a guide line, and what it shows while empty.
struct Chart<'a> {
    title: &'static str,
    about: &'static str,
    lines: Vec<Line<'a>>,
    format: fn(f64) -> String,
    min_top: f64,
    guide: Option<(f64, &'static str)>,
    empty: &'static str,
    /// Extra rows for the hover, at the time hovered.
    extra: Vec<Line<'a>>,
}

impl HealthCharts {
    /// Called once per frame with the time it took to draw.
    pub fn record_frame(&mut self, took: Duration) {
        self.frames.record(took);
    }

    pub fn show(&mut self, ui: &mut Ui, app: &App) {
        let window = &mut self.window;
        let frames = &self.frames.series;
        card_with_header(
            ui,
            "Health",
            window,
            |ui, window| {
                for w in Window::ALL {
                    if ui
                        .selectable_label(*window == w, w.label())
                        .on_hover_text(w.hover())
                        .clicked()
                    {
                        *window = w;
                    }
                }
            },
            |ui, window| charts(ui, app, frames, *window),
        );
    }
}

fn charts(ui: &mut Ui, app: &App, frames: &Series, window: Window) {
    let m = &app.metrics;
    let direct = app.connection.is_direct();
    let index_empty = if direct {
        "Waiting for the index…"
    } else {
        "Needs a direct node"
    };
    let cores = m.cores;
    let charts = [
        Chart {
            title: "CPU",
            about: "This process's CPU use, as a share of all the machine's cores",
            lines: vec![Line {
                name: "CPU",
                series: &m.cpu_percent,
                color: theme::ACCENT,
            }],
            format: |v| format!("{v:.1}%"),
            min_top: 5.0,
            guide: None,
            empty: "Waiting for samples…",
            extra: vec![],
        },
        Chart {
            title: "Memory",
            about: "This process's resident memory",
            lines: vec![Line {
                name: "Resident",
                series: &m.memory_bytes,
                color: theme::ACCENT,
            }],
            format: |v| format_size(v as u64),
            min_top: 64.0 * 1024.0 * 1024.0,
            guide: None,
            empty: "Waiting for samples…",
            extra: vec![],
        },
        Chart {
            title: "Disk I/O",
            about: "Bytes this process read from and wrote to disk per second \
                    (reads served from the OS cache don't count)",
            lines: vec![
                Line {
                    name: "Read",
                    series: &m.disk_read_bps,
                    color: theme::ACCENT,
                },
                Line {
                    name: "Write",
                    series: &m.disk_write_bps,
                    color: theme::WARN,
                },
            ],
            format: |v| format!("{}/s", format_size(v as u64)),
            min_top: 1024.0 * 1024.0,
            guide: None,
            empty: "Waiting for samples…",
            extra: vec![],
        },
        Chart {
            title: "Index size",
            about: "The DAG index's size on disk (measured every few batches)",
            lines: vec![Line {
                name: "On disk",
                series: &m.index_bytes,
                color: theme::ACCENT,
            }],
            format: |v| format_size(v as u64),
            min_top: 1024.0 * 1024.0,
            guide: None,
            empty: index_empty,
            extra: vec![],
        },
        Chart {
            title: "Ingest",
            about: "Transactions indexed per second; the hover adds the responses \
                    waiting for the index writer",
            lines: vec![Line {
                name: "Indexed",
                series: &m.ingest_tps,
                color: theme::ACCENT,
            }],
            format: |v| format!("{} tx/s", format_number(v.round() as u64)),
            min_top: 10.0,
            guide: None,
            empty: index_empty,
            extra: vec![Line {
                name: "Backlog",
                series: &m.backlog,
                color: theme::TEXT_DIM,
            }],
        },
        Chart {
            title: "Index lag",
            about: "How far the index's newest block trails the node's newest \
                    (both block times, so no clock skew)",
            lines: vec![Line {
                name: "Behind node",
                series: &m.index_lag_secs,
                color: theme::ACCENT,
            }],
            format: |v| format!("{v:.1} s"),
            min_top: 5.0,
            guide: None,
            empty: index_empty,
            extra: vec![],
        },
        Chart {
            title: "Node poll",
            about: "The round trip of each poll of the node (its RPCs run together)",
            lines: vec![Line {
                name: "Round trip",
                series: &m.poll_ms,
                color: theme::ACCENT,
            }],
            format: |v| format!("{v:.0} ms"),
            min_top: 20.0,
            guide: None,
            empty: "Not connected",
            extra: vec![],
        },
        Chart {
            title: "Frame time",
            about: "The slowest frame the GUI drew each second, waiting for the app's \
                    state included; the line is a 60 Hz display's budget",
            lines: vec![Line {
                name: "Slowest",
                series: frames,
                color: theme::ACCENT,
            }],
            format: |v| format!("{v:.1} ms"),
            min_top: FRAME_BUDGET_MS * 1.25,
            guide: Some((FRAME_BUDGET_MS, "60 Hz")),
            empty: "Waiting for samples…",
            extra: vec![],
        },
    ];

    let now = clock_ms();
    let from = window.start_ms(now, m.started_ms);
    for (row, chunk) in charts.chunks(COLUMNS).enumerate() {
        if row > 0 {
            ui.add_space(8.0);
        }
        ui.columns(COLUMNS, |columns| {
            for (i, (ui, chart)) in columns.iter_mut().zip(chunk).enumerate() {
                // Room between the charts, kept off the last one's right edge.
                if i + 1 < COLUMNS {
                    ui.set_max_width(ui.available_width() - 12.0);
                }
                ui.push_id(row * COLUMNS + i, |ui| draw(ui, chart, from, now, cores));
            }
        });
    }
}

fn draw(ui: &mut Ui, chart: &Chart, from: u64, now: u64, cores: usize) {
    // Title and the latest value.
    ui.horizontal(|ui| {
        ui.label(RichText::new(chart.title).color(theme::LABEL))
            .on_hover_text(chart.about);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let latest: Vec<String> = chart
                .lines
                .iter()
                .filter_map(|l| {
                    let value = (chart.format)(l.series.latest()?.value);
                    // Two lines: each value led by its line's initial ("R 1 MB/s | W 2 MB/s").
                    Some(match chart.lines.len() {
                        1 => value,
                        _ => format!("{} {value}", &l.name[..1]),
                    })
                })
                .collect();
            let text = if latest.is_empty() {
                "—".to_string()
            } else {
                latest.join(" | ")
            };
            let label = ui.label(RichText::new(text).color(theme::TEXT_BRIGHT));
            if chart.title == "CPU" {
                label.on_hover_text(format!("{cores} logical cores"));
            }
        });
    });

    let size = egui::vec2(ui.available_width(), PLOT_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect.expand(1.0));
    painter.rect_filled(rect, 2.0, theme::SURFACE);
    let baseline = rect.bottom() - 1.0;
    painter.hline(
        rect.x_range(),
        baseline + 0.5,
        Stroke::new(1.0_f32, theme::BORDER_HI),
    );

    let lines: Vec<(&Line, _)> = chart
        .lines
        .iter()
        .map(|l| (l, l.series.points(from)))
        .collect();
    if lines.iter().all(|(_, p)| p.samples.is_empty()) {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            chart.empty,
            egui::TextStyle::Body.resolve(ui.style()),
            theme::TEXT_DIM,
        );
        return;
    }

    let top = chart
        .lines
        .iter()
        .filter_map(|l| l.series.max_since(from))
        .fold(chart.min_top, f64::max)
        * 1.1;
    let span = now.saturating_sub(from).max(1) as f32;
    let x_at = |ms: u64| rect.left() + (ms.saturating_sub(from)) as f32 / span * rect.width();
    let y_at = |v: f64| baseline - (v / top) as f32 * (rect.height() - 4.0);

    if let Some((value, name)) = chart.guide {
        let y = y_at(value);
        painter.hline(rect.x_range(), y, Stroke::new(1.0_f32, theme::BORDER_HI));
        painter.text(
            pos2(rect.right() - 3.0, y - 1.0),
            egui::Align2::RIGHT_BOTTOM,
            name,
            egui::TextStyle::Small.resolve(ui.style()),
            theme::TEXT_DIM,
        );
    }
    // The scale's top, small in the corner.
    painter.text(
        rect.left_top() + egui::vec2(3.0, 2.0),
        egui::Align2::LEFT_TOP,
        (chart.format)(top),
        egui::TextStyle::Small.resolve(ui.style()),
        theme::TEXT_DIM,
    );

    for (i, (line, points)) in lines.iter().enumerate() {
        for run in runs(&points.samples, x_at, y_at) {
            // Only the first line is filled, so two lines stay readable.
            if i == 0 {
                fill_under(&painter, &run, baseline, line.color.gamma_multiply(0.18));
            }
            if run.len() > 1 {
                painter.line(run, Stroke::new(1.5_f32, line.color));
            } else if let Some(p) = run.first() {
                painter.circle_filled(*p, 1.5, line.color);
            }
        }
    }

    if let Some(pointer) = response.hover_pos() {
        painter.vline(
            pointer.x,
            rect.y_range(),
            Stroke::new(1.0_f32, theme::TEXT_DIM),
        );
        let at = from + ((pointer.x - rect.left()) / rect.width() * span) as u64;
        response.on_hover_ui_at_pointer(|ui| {
            let clock = format_clock_ms(at);
            let mean = lines.iter().any(|(_, p)| p.bucketed);
            kv_grid(ui, "health_tip", |ui| {
                kv(ui, "Time (UTC)", clock[..8].to_string());
                let value_at = |line: &Line| line.series.value_at(from, at);
                for line in &chart.lines {
                    let value = value_at(line).map(chart.format);
                    kv(ui, line.name, value.unwrap_or_else(|| "—".into()));
                }
                // Extra rows are counts, whatever the chart's unit.
                for line in &chart.extra {
                    let value = value_at(line).map(|v| format_number(v.round() as u64));
                    kv(ui, line.name, value.unwrap_or_else(|| "—".into()));
                }
                if mean {
                    kv(ui, "Each point", "a minute's mean");
                }
            });
        });
    }
}

/// One pixel column of a line being gathered by [`runs`]: its x, and its lowest and
/// highest samples, each with its index so they keep the order they came in.
struct Column {
    x: f32,
    low: (usize, f64),
    high: (usize, f64),
}

impl Column {
    fn new(x: f32, i: usize, value: f64) -> Self {
        Self {
            x,
            low: (i, value),
            high: (i, value),
        }
    }

    fn add(&mut self, i: usize, value: f64) {
        if value < self.low.1 {
            self.low = (i, value);
        }
        if value > self.high.1 {
            self.high = (i, value);
        }
    }

    /// Its one or two points, in the order they came.
    fn push_to(&self, run: &mut Vec<Pos2>, y_at: &impl Fn(f64) -> f32) {
        let (first, second) = if self.low.0 <= self.high.0 {
            (self.low, self.high)
        } else {
            (self.high, self.low)
        };
        run.push(pos2(self.x, y_at(first.1)));
        if second.0 != first.0 {
            run.push(pos2(self.x, y_at(second.1)));
        }
    }
}

/// The samples as the lines to draw, split where the series broke, with at most two
/// points per pixel column (its lowest and highest, in the order they came), so an
/// hour of samples costs no more to draw than the plot is wide.
fn runs(
    samples: &[Sample],
    x_at: impl Fn(u64) -> f32,
    y_at: impl Fn(f64) -> f32,
) -> Vec<Vec<Pos2>> {
    let mut runs: Vec<Vec<Pos2>> = Vec::new();
    let mut run: Vec<Pos2> = Vec::new();
    let mut column: Option<Column> = None;
    for (i, s) in samples.iter().enumerate() {
        let x = x_at(s.at_ms).round();
        match &mut column {
            Some(c) if c.x == x && !s.new_run => c.add(i, s.value),
            _ => {
                if let Some(c) = column.take() {
                    c.push_to(&mut run, &y_at);
                }
                if s.new_run && !run.is_empty() {
                    runs.push(std::mem::take(&mut run));
                }
                column = Some(Column::new(x, i, s.value));
            }
        }
    }
    if let Some(c) = column {
        c.push_to(&mut run, &y_at);
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

/// Fill between a line and the baseline, one quad per segment (the area isn't convex).
fn fill_under(painter: &egui::Painter, run: &[Pos2], baseline: f32, color: Color32) {
    if run.len() < 2 {
        return;
    }
    let mut mesh = egui::Mesh::default();
    for pair in run.windows(2) {
        let [a, b] = [pair[0], pair[1]];
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(a, color);
        mesh.colored_vertex(b, color);
        mesh.colored_vertex(pos2(b.x, baseline), color);
        mesh.colored_vertex(pos2(a.x, baseline), color);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    painter.add(mesh);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at_ms: u64, value: f64, new_run: bool) -> Sample {
        Sample {
            at_ms,
            value,
            new_run,
        }
    }

    #[test]
    fn runs_keep_two_points_per_column_and_split_at_breaks() {
        // Ten samples in one column (x = ms / 10), then a break.
        let mut samples: Vec<Sample> = (0..10)
            .map(|i| sample(i, [3.0, 9.0, 1.0][i as usize % 3], false))
            .collect();
        samples.push(sample(100, 5.0, true));
        let runs = runs(&samples, |ms| (ms / 10) as f32, |v| v as f32);
        assert_eq!(runs.len(), 2);
        // The highest came before the lowest.
        assert_eq!(runs[0], vec![pos2(0.0, 9.0), pos2(0.0, 1.0)]);
        assert_eq!(runs[1], vec![pos2(10.0, 5.0)]);
    }
}
