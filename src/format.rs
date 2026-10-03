//! Pure display-formatting helpers shared by the UI.

pub fn format_hashrate(hps: f64) -> String {
    if hps >= 1e18 {
        format!("{:.2} EH/s", hps / 1e18)
    } else if hps >= 1e15 {
        format!("{:.2} PH/s", hps / 1e15)
    } else if hps >= 1e12 {
        format!("{:.2} TH/s", hps / 1e12)
    } else if hps >= 1e9 {
        format!("{:.2} GH/s", hps / 1e9)
    } else if hps >= 1e6 {
        format!("{:.2} MH/s", hps / 1e6)
    } else if hps >= 1e3 {
        format!("{:.2} KH/s", hps / 1e3)
    } else {
        format!("{:.2} H/s", hps)
    }
}

pub fn format_usd(value: f64) -> String {
    if value >= 1_000_000_000.0 {
        format!("${:.2}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("${:.2}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("${:.2}K", value / 1_000.0)
    } else {
        format!("${:.2}", value)
    }
}

/// Sompi as KAS with a fixed number of decimals and thousands separators,
/// e.g. `format_kas(1_234_567_800_000.0, 3)` → `"12,345.678"`.
pub fn format_kas(sompi: f64, decimals: usize) -> String {
    let s = format!("{:.*}", decimals, sompi / 1e8);
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let int = crate::rpc::types::format_number(int.parse().unwrap_or(0));
    if frac.is_empty() {
        int
    } else {
        format!("{int}.{frac}")
    }
}

/// A short duration in its two largest units, e.g. `45s`, `3m 05s`, `2h 10m`, `1d 4h`.
pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (days, hours, mins, secs) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else if mins > 0 {
        format!("{mins}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::format_number;

    #[test]
    fn format_duration_uses_two_largest_units() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(400)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(format_duration(Duration::from_secs(7_800)), "2h 10m");
        assert_eq!(format_duration(Duration::from_secs(100_800)), "1d 4h");
    }

    #[test]
    fn format_kas_decimals_and_separators() {
        assert_eq!(format_kas(1_234_567_800_000.0, 3), "12,345.678");
        assert_eq!(format_kas(1_500.0, 6), "0.000015");
        assert_eq!(format_kas(100_000_000.0, 0), "1");
    }

    #[test]
    fn format_number_zero() {
        assert_eq!(format_number(0), "0");
    }

    #[test]
    fn format_number_small() {
        assert_eq!(format_number(1), "1");
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn format_number_thousands() {
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(12_345), "12,345");
    }

    #[test]
    fn format_number_millions() {
        assert_eq!(format_number(1_000_000), "1,000,000");
        assert_eq!(format_number(123_456_789), "123,456,789");
    }

    #[test]
    fn format_number_large() {
        assert_eq!(format_number(1_000_000_000_000), "1,000,000,000,000");
    }

    // --- format_usd ---

    #[test]
    fn format_usd_billions() {
        assert_eq!(format_usd(3_800_000_000.0), "$3.80B");
    }

    #[test]
    fn format_usd_millions() {
        assert_eq!(format_usd(50_000_000.0), "$50.00M");
    }

    #[test]
    fn format_usd_thousands() {
        assert_eq!(format_usd(1_500.0), "$1.50K");
    }

    #[test]
    fn format_usd_small() {
        assert_eq!(format_usd(42.50), "$42.50");
    }

    // --- format_hashrate ---

    #[test]
    fn format_hashrate_ph() {
        assert_eq!(format_hashrate(1.5e15), "1.50 PH/s");
    }

    #[test]
    fn format_hashrate_th() {
        assert_eq!(format_hashrate(500e12), "500.00 TH/s");
    }

    #[test]
    fn format_hashrate_gh() {
        assert_eq!(format_hashrate(2.5e9), "2.50 GH/s");
    }

    #[test]
    fn format_hashrate_mh() {
        assert_eq!(format_hashrate(100e6), "100.00 MH/s");
    }

    #[test]
    fn format_hashrate_small() {
        assert_eq!(format_hashrate(500.0), "500.00 H/s");
    }
}
