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

pub fn truncate_hash(hash: &str) -> String {
    let char_count = hash.chars().count();
    if char_count > 24 {
        let prefix: String = hash.chars().take(12).collect();
        let suffix: String = hash.chars().skip(char_count - 12).collect();
        format!("{}...{}", prefix, suffix)
    } else {
        hash.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::format_number;

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

    // --- truncate_hash ---

    #[test]
    fn truncate_hash_short() {
        assert_eq!(truncate_hash("abcdef"), "abcdef");
    }

    #[test]
    fn truncate_hash_exactly_24() {
        let hash = "a".repeat(24);
        assert_eq!(truncate_hash(&hash), hash);
    }

    #[test]
    fn truncate_hash_long() {
        let hash = "abcdefghijklmnopqrstuvwxyz0123456789";
        let result = truncate_hash(hash);
        // first 12 + "..." + last 12
        assert_eq!(result, "abcdefghijkl...yz0123456789");
        assert_eq!(result.len(), 27);
    }

    #[test]
    fn truncate_hash_empty() {
        assert_eq!(truncate_hash(""), "");
    }

    #[test]
    fn truncate_hash_realistic() {
        let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let result = truncate_hash(hash);
        assert_eq!(result.len(), 27); // 12 + 3 + 12
        assert!(result.starts_with("abcdef123456"));
        assert!(result.ends_with("ef1234567890"));
        assert!(result.contains("..."));
    }

}
