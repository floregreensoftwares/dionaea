//! Which performance events a site keeps when it keeps only a share.
//!
//! The choice is a function of the event id alone: an event sent again (a
//! retry, a replay of the spool) is kept or dropped the same way, and no
//! node has to remember anything. The ids are random, so the share kept
//! follows the rate; the id is mixed first so a source of ids that is not
//! uniform in its low bits cannot skew it.

use dionaea_meta::SAMPLE_FULL_PPM;

/// SplitMix64's finalizer: every input bit affects every output bit.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Whether the event with this id is kept when `ppm` millionths are.
pub(crate) fn keep(event_key: u128, ppm: u32) -> bool {
    if ppm >= SAMPLE_FULL_PPM {
        return true;
    }
    let bits = mix((event_key as u64) ^ mix((event_key >> 64) as u64));
    // A uniform value in [0, 1_000_000): the high 64 bits of bits * 1e6.
    let position = (u128::from(bits) * u128::from(SAMPLE_FULL_PPM)) >> 64;
    position < u128::from(ppm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed stream of ids that are not random in their low bits: counters
    /// in the high half and in the low half.
    fn ids(count: u64) -> impl Iterator<Item = u128> {
        (0..count).map(|n| (u128::from(n) << 64) | u128::from(n * 7))
    }

    #[test]
    fn the_whole_rate_keeps_everything_and_the_smallest_keeps_about_a_millionth() {
        assert!(ids(1_000).all(|id| keep(id, SAMPLE_FULL_PPM)));
        let kept = ids(2_000_000).filter(|id| keep(*id, 1)).count();
        assert!(kept <= 8, "{kept} of 2,000,000 at one in a million");
    }

    #[test]
    fn the_share_kept_follows_the_rate() {
        let total = 200_000u64;
        for ppm in [10_000u32, 250_000, 500_000, 900_000] {
            let kept = ids(total).filter(|id| keep(*id, ppm)).count() as f64;
            let expected = f64::from(ppm) / 1_000_000.0 * total as f64;
            // Four standard deviations of a binomial count.
            let spread = 4.0 * (expected * (1.0 - f64::from(ppm) / 1_000_000.0)).sqrt();
            assert!(
                (kept - expected).abs() <= spread,
                "{ppm}: kept {kept}, expected {expected} within {spread}"
            );
        }
    }

    #[test]
    fn the_choice_is_the_same_every_time_and_a_lower_rate_keeps_a_subset() {
        for id in ids(20_000) {
            assert_eq!(keep(id, 300_000), keep(id, 300_000));
            if keep(id, 100_000) {
                assert!(
                    keep(id, 300_000),
                    "the events of a lower rate stay at a higher one"
                );
            }
        }
    }
}
