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

/// A unix-millisecond timestamp as UTC and how long ago, `2026-10-08T12:34:56Z (5m ago)`.
pub fn format_when(ms: u64) -> String {
    let ago = std::time::Duration::from_millis(now_ms().saturating_sub(ms));
    format!("{} ({} ago)", format_utc(ms), format_duration(ago))
}

/// A unix-millisecond timestamp as UTC, `2026-10-08T12:34:56Z`.
pub fn format_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// The inverse of [`format_utc`]: `2026-10-08T12:34:56Z`, `2026-10-08T12:34Z`,
/// `2026-10-08` (midnight), with an optional `.123` fraction, as unix milliseconds. UTC
/// only; `None` for anything else.
pub fn parse_utc(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches(['Z', 'z']);
    let (date, time) = match s.split_once(['T', 't', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut h, mut m, mut sec, mut ms) = (0u64, 0u64, 0u64, 0u64);
    if let Some(time) = time {
        let (hms, frac) = match time.split_once('.') {
            Some((hms, frac)) => (hms, Some(frac)),
            None => (time, None),
        };
        let mut parts = hms.split(':');
        h = parts.next()?.parse().ok()?;
        m = parts.next()?.parse().ok()?;
        sec = parts
            .next()
            .map(|p| p.parse())
            .transpose()
            .ok()?
            .unwrap_or(0);
        if parts.next().is_some() || h > 23 || m > 59 || sec > 59 {
            return None;
        }
        if let Some(frac) = frac {
            if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            ms = format!("{frac:0<3}").parse().ok()?;
        }
    }
    // Days from civil (Howard Hinnant's algorithm), the inverse of `format_utc`'s.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86_400 + h * 3_600 + m * 60 + sec) * 1_000 + ms)
}

/// A duration such as `24h`, `1d12h`, `90m`, `1h30m15s`, `500ms` or `2w` in
/// milliseconds; `None` for anything else (no unit, an unknown unit, no digits).
pub fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut unit = String::new();
    let mut flush = |digits: &mut String, unit: &mut String| -> Option<()> {
        if digits.is_empty() {
            return None;
        }
        let n: u64 = digits.parse().ok()?;
        let scale = match unit.as_str() {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "w" => 7 * 86_400_000,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(scale)?)?;
        digits.clear();
        unit.clear();
        Some(())
    };
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !unit.is_empty() {
                flush(&mut digits, &mut unit)?;
            }
            digits.push(c);
        } else if c.is_ascii_alphabetic() {
            unit.push(c);
        } else {
            return None;
        }
    }
    flush(&mut digits, &mut unit)?;
    Some(total)
}

/// A duration in milliseconds in its units, largest first: `1d2h`, `90m` → `1h30m`,
/// `1500ms` → `1s500ms`, `0` → `0s`. The inverse of [`parse_duration_ms`].
pub fn format_duration_ms(ms: u64) -> String {
    if ms == 0 {
        return "0s".to_string();
    }
    let mut out = String::new();
    let mut rest = ms;
    for (scale, unit) in [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "m"),
        (1_000, "s"),
        (1, "ms"),
    ] {
        let n = rest / scale;
        if n > 0 {
            out.push_str(&format!("{n}{unit}"));
            rest %= scale;
        }
    }
    out
}

/// A KAS amount written as a decimal (`1.5`, `-0.00000001`, `1,000`) as sompi, exactly:
/// no floating point. `None` for more than 8 decimals or anything that isn't a number.
pub fn parse_kas_to_sompi(s: &str) -> Option<i64> {
    let s = s.trim().replace([',', '_'], "");
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(&s)),
    };
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if (int.is_empty() && frac.is_empty())
        || frac.len() > 8
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int: i64 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let frac: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<8}").parse().ok()?
    };
    let sompi = int.checked_mul(100_000_000)?.checked_add(frac)?;
    Some(if negative { -sompi } else { sompi })
}

/// Sompi as an exact KAS decimal with no trailing zeros: `150000000` → `1.5`,
/// `1` → `0.00000001`, `-200000000` → `-2`. The inverse of [`parse_kas_to_sompi`].
pub fn format_sompi_exact(sompi: i64) -> String {
    let sign = if sompi < 0 { "-" } else { "" };
    let abs = sompi.unsigned_abs();
    let (int, frac) = (abs / 100_000_000, abs % 100_000_000);
    if frac == 0 {
        format!("{sign}{int}")
    } else {
        let frac = format!("{frac:08}");
        format!("{sign}{int}.{}", frac.trim_end_matches('0'))
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

/// The transaction page on Kaspa Explorer, on the testnet-10 explorer if `testnet`.
pub fn explorer_tx_url(txid: &str, testnet: bool) -> String {
    format!("https://{}/txs/{txid}", explorer_host(testnet))
}

/// The transaction page on Kaspa Stream (mainnet).
pub fn kaspa_stream_tx_url(txid: &str) -> String {
    format!("https://kaspa.stream/transactions/{txid}")
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
        assert_eq!(
            explorer_tx_url("ab12", true),
            "https://explorer-tn10.kaspa.org/txs/ab12"
        );
        assert_eq!(
            kaspa_stream_tx_url("ab12"),
            "https://kaspa.stream/transactions/ab12"
        );
    }

    #[test]
    fn format_utc_matches_known_dates() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_759_926_896_123), "2025-10-08T12:34:56Z");
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

    #[test]
    fn parse_utc_inverts_format_utc() {
        for ms in [
            0u64,
            1_000,
            1_700_000_000_123,
            4_102_444_800_000,
            253_402_300_799_000,
        ] {
            let text = format_utc(ms);
            assert_eq!(parse_utc(&text), Some(ms / 1000 * 1000), "{text}");
        }
        assert_eq!(
            parse_utc("2026-10-08T12:34:56.123Z"),
            Some(1_791_462_896_123)
        );
        assert_eq!(parse_utc("2026-10-08T12:34:56.1Z"), Some(1_791_462_896_100));
        assert_eq!(parse_utc("2026-10-08T12:34Z"), Some(1_791_462_840_000));
        assert_eq!(parse_utc("2026-10-08"), Some(1_791_417_600_000));
        assert_eq!(parse_utc("1970-01-01"), Some(0));
        assert_eq!(parse_utc("2026-13-01"), None);
        assert_eq!(parse_utc("2026-10-08T25:00Z"), None);
        assert_eq!(parse_utc("yesterday"), None);
        assert_eq!(parse_utc("1969-12-31"), None);
    }

    #[test]
    fn durations_parse_and_print() {
        assert_eq!(parse_duration_ms("24h"), Some(86_400_000));
        assert_eq!(parse_duration_ms("1d12h"), Some(129_600_000));
        assert_eq!(parse_duration_ms("90m"), Some(5_400_000));
        assert_eq!(parse_duration_ms("1h30m15s"), Some(5_415_000));
        assert_eq!(parse_duration_ms("500ms"), Some(500));
        assert_eq!(parse_duration_ms("2W"), Some(1_209_600_000));
        assert_eq!(parse_duration_ms("24"), None);
        assert_eq!(parse_duration_ms("h"), None);
        assert_eq!(parse_duration_ms("3y"), None);
        assert_eq!(parse_duration_ms(""), None);
        for ms in [0u64, 1, 999, 1_000, 5_415_000, 129_600_000, 90_061_001] {
            let text = format_duration_ms(ms);
            assert_eq!(parse_duration_ms(&text), Some(ms), "{text}");
        }
        assert_eq!(format_duration_ms(90_061_001), "1d1h1m1s1ms");
        assert_eq!(format_duration_ms(0), "0s");
    }

    #[test]
    fn kas_amounts_are_exact() {
        assert_eq!(parse_kas_to_sompi("1.5"), Some(150_000_000));
        assert_eq!(parse_kas_to_sompi("0.00000001"), Some(1));
        assert_eq!(parse_kas_to_sompi("-2"), Some(-200_000_000));
        assert_eq!(parse_kas_to_sompi("1,000.25"), Some(100_025_000_000));
        assert_eq!(parse_kas_to_sompi(".5"), Some(50_000_000));
        assert_eq!(parse_kas_to_sompi("0.000000001"), None);
        assert_eq!(parse_kas_to_sompi("abc"), None);
        assert_eq!(parse_kas_to_sompi(""), None);
        for sompi in [0i64, 1, 150_000_000, -200_000_000, 123_456_789_012_345] {
            let text = format_sompi_exact(sompi);
            assert_eq!(parse_kas_to_sompi(&text), Some(sompi), "{text}");
        }
        assert_eq!(format_sompi_exact(150_000_000), "1.5");
        assert_eq!(format_sompi_exact(1), "0.00000001");
        assert_eq!(format_sompi_exact(-200_000_000), "-2");
    }
}
