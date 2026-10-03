//! Pure display-formatting helpers, shared by the GUI and the CLI.

/// Milliseconds since the Unix epoch, by the local clock.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

pub fn sompi_to_kas(sompi: u64) -> f64 {
    sompi as f64 / 1e8
}

/// An integer with thousands separators, e.g. `12,345`.
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// `value` divided by the largest `(threshold, suffix)` it reaches (thresholds in
/// descending order), with its suffix: `scaled(1.5e15, …)` → `(1.5, "P")`.
fn scaled(value: f64, units: &[(f64, &'static str)]) -> (f64, &'static str) {
    units
        .iter()
        .find(|(threshold, _)| value >= *threshold)
        .map_or((value, ""), |&(threshold, suffix)| {
            (value / threshold, suffix)
        })
}

pub fn format_hashrate(hps: f64) -> String {
    const UNITS: [(f64, &str); 6] = [
        (1e18, "E"),
        (1e15, "P"),
        (1e12, "T"),
        (1e9, "G"),
        (1e6, "M"),
        (1e3, "K"),
    ];
    let (value, prefix) = scaled(hps, &UNITS);
    format!("{value:.2} {prefix}H/s")
}

pub fn format_usd(value: f64) -> String {
    const UNITS: [(f64, &str); 3] = [(1e9, "B"), (1e6, "M"), (1e3, "K")];
    let (value, suffix) = scaled(value, &UNITS);
    format!("${value:.2}{suffix}")
}

/// Sompi as KAS with a fixed number of decimals and thousands separators,
/// e.g. `format_kas(1_234_567_800_000.0, 3)` → `"12,345.678"`.
pub fn format_kas(sompi: f64, decimals: usize) -> String {
    let s = format!("{:.*}", decimals, sompi / 1e8);
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let int = format_number(int.parse().unwrap_or(0));
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

/// `s` unchanged if it has at most `max_chars` characters, otherwise its start and end
/// joined by `...` in `max_chars` characters, e.g. `kaspa:qzv6...3gujgy`.
pub fn shorten_middle(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let (head, tail) = (keep.div_ceil(2), keep / 2);
    let start: String = s.chars().take(head).collect();
    let end: String = s.chars().skip(len - tail).collect();
    format!("{start}...{end}")
}

/// Kaspa Explorer's host: the testnet-10 explorer if `testnet`.
fn explorer_host(testnet: bool) -> &'static str {
    if testnet {
        "explorer-tn10.kaspa.org"
    } else {
        "explorer.kaspa.org"
    }
}

/// The address page on Kaspa Explorer: the testnet-10 explorer for `kaspatest:` addresses.
pub fn explorer_address_url(addr: &str) -> String {
    let host = explorer_host(addr.starts_with("kaspatest:"));
    format!("https://{host}/addresses/{addr}")
}

/// The address page on Kaspa Stream (mainnet).
pub fn kaspa_stream_address_url(addr: &str) -> String {
    format!("https://kaspa.stream/addresses/{addr}")
}

/// The block page on Kaspa Explorer, on the testnet-10 explorer if `testnet`.
pub fn explorer_block_url(hash: &str, testnet: bool) -> String {
    format!("https://{}/blocks/{hash}", explorer_host(testnet))
}

/// The block page on Kaspa Stream (mainnet).
pub fn kaspa_stream_block_url(hash: &str) -> String {
    format!("https://kaspa.stream/blocks/{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_urls_by_network() {
        assert_eq!(
            explorer_address_url("kaspa:qabc"),
            "https://explorer.kaspa.org/addresses/kaspa:qabc"
        );
        assert_eq!(
            explorer_address_url("kaspatest:qabc"),
            "https://explorer-tn10.kaspa.org/addresses/kaspatest:qabc"
        );
        assert_eq!(
            kaspa_stream_address_url("kaspa:qabc"),
            "https://kaspa.stream/addresses/kaspa:qabc"
        );
        assert_eq!(
            explorer_block_url("ab12", false),
            "https://explorer.kaspa.org/blocks/ab12"
        );
        assert_eq!(
            explorer_block_url("ab12", true),
            "https://explorer-tn10.kaspa.org/blocks/ab12"
        );
        assert_eq!(
            kaspa_stream_block_url("ab12"),
            "https://kaspa.stream/blocks/ab12"
        );
    }

    #[test]
    fn shorten_middle_keeps_text_that_fits() {
        assert_eq!(shorten_middle("kaspa:abc", 9), "kaspa:abc");
        assert_eq!(shorten_middle("kaspa:abc", 20), "kaspa:abc");
    }

    #[test]
    fn shorten_middle_keeps_start_and_end() {
        let addr = "kaspa:qzv6abcdefghijklmnop3gujgy";
        let short = shorten_middle(addr, 19);
        assert_eq!(short, "kaspa:qz...op3gujgy");
        assert!(short.chars().count() <= 19);
        assert_eq!(shorten_middle(addr, 3), "...");
    }

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

    #[test]
    fn sompi_to_kas_converts() {
        assert_eq!(sompi_to_kas(0), 0.0);
        assert_eq!(sompi_to_kas(100_000_000), 1.0);
        assert!((sompi_to_kas(50_000_000) - 0.5).abs() < f64::EPSILON);
        assert!((sompi_to_kas(2_900_000_000_000_000_000) - 29_000_000_000.0).abs() < 1.0);
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
