//! Kaspa's block reward (subsidy) schedule, ported from rusty-kaspa's coinbase manager
//! (`consensus/src/processes/coinbase.rs`, rev `01b532e`) and consensus params. The
//! reward drops every "month" (1/12 of 365.25 days of DAA score), halving each year.

use std::str::FromStr;
use std::time::Duration;

use kaspa_rpc_core::RpcAddress;
use kaspa_wrpc_client::prelude::NetworkId;

/// The mainnet burn address: a P2PK script on an all-zero public key, which nobody can
/// sign for, so coins sent there are unspendable.
pub const BURN_ADDRESS: &str =
    "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e";

/// [`BURN_ADDRESS`] with the address prefix of `network_id` (e.g. `kaspatest:` on
/// testnet-10). `None` for an unknown network.
pub fn burn_address(network_id: &str) -> Option<RpcAddress> {
    let mut address = RpcAddress::try_from(BURN_ADDRESS).ok()?;
    address.prefix = NetworkId::from_str(network_id).ok()?.into();
    Some(address)
}

/// A month of 365.25 / 12 days, in seconds.
const SECONDS_PER_MONTH: u64 = 2_629_800;

/// Before this DAA score the reward was a flat pre-deflationary subsidy. Half a year of
/// seconds minus the three days the network was down after launch.
const DEFLATIONARY_PHASE_DAA_SCORE: u64 = 15_778_800 - 259_200;
const PRE_DEFLATIONARY_SUBSIDY: u64 = 50_000_000_000;

/// Blocks per second before and after the Crescendo hard fork.
const BPS_BEFORE: u64 = 1;
const BPS_AFTER: u64 = 10;

/// The schedule of a network: when Crescendo (1 → 10 BPS) activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emission {
    crescendo_daa_score: u64,
}

/// The current block reward and when it next drops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockReward {
    /// Current reward per block, in sompi.
    pub sompi: u64,
    /// The reward per block after the next reduction, in sompi.
    pub next_sompi: u64,
    /// DAA score at which the next reduction happens.
    pub next_daa_score: u64,
    /// Estimated time until then, at the current block rate.
    pub next_in: Duration,
}

impl Emission {
    /// The schedule for a network ID such as `mainnet` or `testnet-10`; `None` for
    /// networks whose schedule isn't known (e.g. other testnets, devnet, simnet).
    pub fn for_network(network_id: &str) -> Option<Self> {
        let crescendo_daa_score = match network_id {
            "mainnet" => 110_165_000,
            "testnet-10" => 88_657_000,
            _ => return None,
        };
        Some(Self {
            crescendo_daa_score,
        })
    }

    fn bps(&self, daa_score: u64) -> u64 {
        if daa_score >= self.crescendo_daa_score {
            BPS_AFTER
        } else {
            BPS_BEFORE
        }
    }

    /// Seconds of deflationary phase up to `daa_score` (≥ the deflationary phase).
    fn seconds(&self, daa_score: u64) -> u64 {
        let start = DEFLATIONARY_PHASE_DAA_SCORE;
        if daa_score < self.crescendo_daa_score {
            (daa_score - start) / BPS_BEFORE
        } else {
            (self.crescendo_daa_score - start) / BPS_BEFORE
                + (daa_score - self.crescendo_daa_score) / BPS_AFTER
        }
    }

    /// The DAA score at which `seconds` of deflationary phase have passed (inverse of
    /// [`Self::seconds`]).
    fn daa_score_at(&self, seconds: u64) -> u64 {
        let before = (self.crescendo_daa_score - DEFLATIONARY_PHASE_DAA_SCORE) / BPS_BEFORE;
        if seconds < before {
            DEFLATIONARY_PHASE_DAA_SCORE + seconds * BPS_BEFORE
        } else {
            self.crescendo_daa_score + (seconds - before) * BPS_AFTER
        }
    }

    fn subsidy(month: u64, bps: u64) -> u64 {
        let month = (month as usize).min(SUBSIDY_BY_MONTH_TABLE.len() - 1);
        SUBSIDY_BY_MONTH_TABLE[month].div_ceil(bps)
    }

    /// The reward per block at `daa_score`, in sompi (rusty-kaspa's `calc_block_subsidy`).
    pub fn block_subsidy(&self, daa_score: u64) -> u64 {
        if daa_score < DEFLATIONARY_PHASE_DAA_SCORE {
            return PRE_DEFLATIONARY_SUBSIDY / self.bps(daa_score);
        }
        Self::subsidy(
            self.seconds(daa_score) / SECONDS_PER_MONTH,
            self.bps(daa_score),
        )
    }

    /// The current reward at `daa_score` and the next reduction. `None` in the
    /// pre-deflationary phase or once the reward has run out.
    pub fn block_reward(&self, daa_score: u64) -> Option<BlockReward> {
        if daa_score < DEFLATIONARY_PHASE_DAA_SCORE {
            return None;
        }
        let sompi = self.block_subsidy(daa_score);
        if sompi == 0 {
            return None;
        }
        let month = self.seconds(daa_score) / SECONDS_PER_MONTH;
        let next_daa_score = self.daa_score_at((month + 1) * SECONDS_PER_MONTH);
        let bps = self.bps(daa_score);
        Some(BlockReward {
            sompi,
            next_sompi: self.block_subsidy(next_daa_score),
            next_daa_score,
            next_in: Duration::from_secs((next_daa_score - daa_score) / bps),
        })
    }
}

/// Reward per second for each month of the deflationary phase (= reward per block at
/// 1 BPS), in sompi. Copied verbatim from rusty-kaspa.
#[rustfmt::skip]
const SUBSIDY_BY_MONTH_TABLE: [u64; 426] = [
    44000000000, 41530469757, 39199543598, 36999442271, 34922823143, 32962755691, 31112698372, 29366476791, 27718263097, 26162556530, 24694165062, 23308188075, 22000000000, 20765234878, 19599771799, 18499721135, 17461411571, 16481377845, 15556349186, 14683238395, 13859131548, 13081278265, 12347082531, 11654094037, 11000000000,
    10382617439, 9799885899, 9249860567, 8730705785, 8240688922, 7778174593, 7341619197, 6929565774, 6540639132, 6173541265, 5827047018, 5500000000, 5191308719, 4899942949, 4624930283, 4365352892, 4120344461, 3889087296, 3670809598, 3464782887, 3270319566, 3086770632, 2913523509, 2750000000, 2595654359,
    2449971474, 2312465141, 2182676446, 2060172230, 1944543648, 1835404799, 1732391443, 1635159783, 1543385316, 1456761754, 1375000000, 1297827179, 1224985737, 1156232570, 1091338223, 1030086115, 972271824, 917702399, 866195721, 817579891, 771692658, 728380877, 687500000, 648913589, 612492868,
    578116285, 545669111, 515043057, 486135912, 458851199, 433097860, 408789945, 385846329, 364190438, 343750000, 324456794, 306246434, 289058142, 272834555, 257521528, 243067956, 229425599, 216548930, 204394972, 192923164, 182095219, 171875000, 162228397, 153123217, 144529071,
    136417277, 128760764, 121533978, 114712799, 108274465, 102197486, 96461582, 91047609, 85937500, 81114198, 76561608, 72264535, 68208638, 64380382, 60766989, 57356399, 54137232, 51098743, 48230791, 45523804, 42968750, 40557099, 38280804, 36132267, 34104319,
    32190191, 30383494, 28678199, 27068616, 25549371, 24115395, 22761902, 21484375, 20278549, 19140402, 18066133, 17052159, 16095095, 15191747, 14339099, 13534308, 12774685, 12057697, 11380951, 10742187, 10139274, 9570201, 9033066, 8526079, 8047547,
    7595873, 7169549, 6767154, 6387342, 6028848, 5690475, 5371093, 5069637, 4785100, 4516533, 4263039, 4023773, 3797936, 3584774, 3383577, 3193671, 3014424, 2845237, 2685546, 2534818, 2392550, 2258266, 2131519, 2011886, 1898968,
    1792387, 1691788, 1596835, 1507212, 1422618, 1342773, 1267409, 1196275, 1129133, 1065759, 1005943, 949484, 896193, 845894, 798417, 753606, 711309, 671386, 633704, 598137, 564566, 532879, 502971, 474742, 448096,
    422947, 399208, 376803, 355654, 335693, 316852, 299068, 282283, 266439, 251485, 237371, 224048, 211473, 199604, 188401, 177827, 167846, 158426, 149534, 141141, 133219, 125742, 118685, 112024, 105736,
    99802, 94200, 88913, 83923, 79213, 74767, 70570, 66609, 62871, 59342, 56012, 52868, 49901, 47100, 44456, 41961, 39606, 37383, 35285, 33304, 31435, 29671, 28006, 26434, 24950,
    23550, 22228, 20980, 19803, 18691, 17642, 16652, 15717, 14835, 14003, 13217, 12475, 11775, 11114, 10490, 9901, 9345, 8821, 8326, 7858, 7417, 7001, 6608, 6237, 5887,
    5557, 5245, 4950, 4672, 4410, 4163, 3929, 3708, 3500, 3304, 3118, 2943, 2778, 2622, 2475, 2336, 2205, 2081, 1964, 1854, 1750, 1652, 1559, 1471, 1389,
    1311, 1237, 1168, 1102, 1040, 982, 927, 875, 826, 779, 735, 694, 655, 618, 584, 551, 520, 491, 463, 437, 413, 389, 367, 347, 327,
    309, 292, 275, 260, 245, 231, 218, 206, 194, 183, 173, 163, 154, 146, 137, 130, 122, 115, 109, 103, 97, 91, 86, 81, 77,
    73, 68, 65, 61, 57, 54, 51, 48, 45, 43, 40, 38, 36, 34, 32, 30, 28, 27, 25, 24, 22, 21, 20, 19, 18,
    17, 16, 15, 14, 13, 12, 12, 11, 10, 10, 9, 9, 8, 8, 7, 7, 6, 6, 6, 5, 5, 5, 4, 4, 4,
    4, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burn_address_follows_the_network() {
        assert_eq!(burn_address("mainnet").unwrap().to_string(), BURN_ADDRESS);
        let testnet = burn_address("testnet-10").unwrap().to_string();
        assert!(testnet.starts_with("kaspatest:qqqqqqqq"));
        assert!(burn_address("nonsense").is_none());
    }

    #[test]
    fn unknown_networks_have_no_schedule() {
        assert!(Emission::for_network("mainnet").is_some());
        assert!(Emission::for_network("testnet-10").is_some());
        assert!(Emission::for_network("testnet-12").is_none());
        assert!(Emission::for_network("devnet").is_none());
    }

    #[test]
    fn subsidy_matches_rusty_kaspa() {
        let e = Emission::for_network("mainnet").unwrap();
        // Pre-deflationary: 500 KAS per block at 1 BPS.
        assert_eq!(e.block_subsidy(0), 50_000_000_000);
        // First deflationary month: 440 KAS.
        assert_eq!(
            e.block_subsidy(DEFLATIONARY_PHASE_DAA_SCORE),
            44_000_000_000
        );
        // Just after Crescendo: the 1 BPS month's reward split over 10 blocks (rounded up).
        let month = (e.crescendo_daa_score - DEFLATIONARY_PHASE_DAA_SCORE) / SECONDS_PER_MONTH;
        assert_eq!(
            e.block_subsidy(e.crescendo_daa_score),
            SUBSIDY_BY_MONTH_TABLE[month as usize].div_ceil(10)
        );
        // Far in the future the reward runs out.
        assert_eq!(e.block_subsidy(u64::MAX / 2), 0);
        assert!(e.block_reward(u64::MAX / 2).is_none());
    }

    #[test]
    fn next_reduction_is_the_next_month() {
        let e = Emission::for_network("mainnet").unwrap();
        let daa_score = 400_000_000;
        let reward = e.block_reward(daa_score).unwrap();
        assert!(reward.next_daa_score > daa_score);
        assert!(reward.next_sompi < reward.sompi);
        // The reward changes exactly at the next reduction.
        assert_eq!(e.block_subsidy(reward.next_daa_score - 1), reward.sompi);
        assert_eq!(e.block_subsidy(reward.next_daa_score), reward.next_sompi);
        assert_eq!(
            reward.next_in,
            Duration::from_secs((reward.next_daa_score - daa_score) / 10)
        );
        // At most a month away.
        assert!(reward.next_in <= Duration::from_secs(SECONDS_PER_MONTH));
    }

    #[test]
    fn next_reduction_before_crescendo() {
        let e = Emission::for_network("mainnet").unwrap();
        let reward = e.block_reward(DEFLATIONARY_PHASE_DAA_SCORE + 10).unwrap();
        assert_eq!(
            reward.next_daa_score,
            DEFLATIONARY_PHASE_DAA_SCORE + SECONDS_PER_MONTH
        );
        assert_eq!(e.block_subsidy(reward.next_daa_score), 41_530_469_757);
    }
}
