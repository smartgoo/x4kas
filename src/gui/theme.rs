use eframe::egui::Color32;

use crate::app::{ConnectionStatus, DaemonStatus};

pub const ACCENT: Color32 = Color32::from_rgb(0x49, 0xc5, 0xb6);
pub const OK: Color32 = Color32::from_rgb(0x4c, 0xaf, 0x50);
pub const WARN: Color32 = Color32::from_rgb(0xe0, 0xa8, 0x2e);
pub const ERROR: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);

pub fn connection_status(status: &ConnectionStatus) -> (&'static str, Color32) {
    match status {
        ConnectionStatus::Connected => ("Connected", OK),
        ConnectionStatus::Connecting => ("Connecting…", WARN),
        ConnectionStatus::Disconnected => ("Disconnected", ERROR),
        ConnectionStatus::Error(_) => ("Error", ERROR),
    }
}

pub fn daemon_status(status: &DaemonStatus) -> (&'static str, Color32) {
    match status {
        DaemonStatus::Stopped => ("Stopped", Color32::GRAY),
        DaemonStatus::Starting => ("Starting…", WARN),
        DaemonStatus::Running => ("Running", OK),
        DaemonStatus::Stopping => ("Stopping…", WARN),
        DaemonStatus::Error(_) => ("Error", ERROR),
    }
}
