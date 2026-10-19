//! How much of a site's performance data is kept. A rate is stored in
//! millionths (1 to 1,000,000), so no decimal text reaches the ingest path.
//! A change is part of what the ingest cache holds: it bumps the generation,
//! so every server applies it at its next refresh.

use rusqlite::params;

use crate::MetaError;
use crate::sqlite::{SqliteMeta, bump, storage};

/// Every performance event kept: the default.
pub const SAMPLE_FULL_PPM: u32 = 1_000_000;

/// The millionths for a rate such as `0.25`, when it is in `(0, 1]` and has
/// at most six decimals.
pub fn ppm_from_rate(rate: f64) -> Option<u32> {
    if !rate.is_finite() || rate <= 0.0 || rate > 1.0 {
        return None;
    }
    let scaled = rate * f64::from(SAMPLE_FULL_PPM);
    let ppm = scaled.round();
    // 0.1234567 is more precision than is kept: say so instead of rounding.
    if (scaled - ppm).abs() > 1e-4 {
        return None;
    }
    // In (0, 1e6] by the checks above.
    let ppm = ppm as u32;
    (1..=SAMPLE_FULL_PPM).contains(&ppm).then_some(ppm)
}

pub trait SiteSampling: Send + Sync {
    /// Keeps `ppm` millionths of the site's performance events (see
    /// [`SAMPLE_FULL_PPM`]); errors and crashes are never sampled.
    fn set_performance_sampling(&self, site_id: u32, ppm: u32) -> Result<(), MetaError>;

    /// The millionths kept; [`SAMPLE_FULL_PPM`] unless the site has its own.
    fn performance_sampling(&self, site_id: u32) -> Result<u32, MetaError>;
}

impl SiteSampling for SqliteMeta {
    fn set_performance_sampling(&self, site_id: u32, ppm: u32) -> Result<(), MetaError> {
        if !(1..=SAMPLE_FULL_PPM).contains(&ppm) {
            return Err(MetaError::Invalid(
                "the rate kept is from 0.000001 to 1".to_owned(),
            ));
        }
        self.write(|tx, _| {
            let changed = tx
                .execute(
                    "UPDATE sites SET performance_sample_ppm = ?1 WHERE id = ?2",
                    params![ppm, site_id],
                )
                .map_err(storage)?;
            if changed == 0 {
                return Err(MetaError::NotFound("site"));
            }
            bump(tx)
        })
    }

    fn performance_sampling(&self, site_id: u32) -> Result<u32, MetaError> {
        self.read(|tx| {
            let ppm: Option<i64> = tx
                .query_row(
                    "SELECT performance_sample_ppm FROM sites WHERE id = ?1",
                    [site_id],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
                .map_err(storage)?;
            let ppm = ppm.ok_or(MetaError::NotFound("site"))?;
            u32::try_from(ppm)
                .ok()
                .filter(|p| (1..=SAMPLE_FULL_PPM).contains(p))
                .ok_or_else(|| {
                    MetaError::Corrupt("a stored sample rate is out of range".to_owned())
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MetaStore;

    fn site() -> (SqliteMeta, u32) {
        let meta = SqliteMeta::open_in_memory().expect("open");
        let org = meta.create_organization("acme").expect("org");
        let site = meta.create_site(org.id, "app").expect("site").id;
        (meta, site)
    }

    #[test]
    fn rates_become_millionths_or_are_refused() {
        assert_eq!(ppm_from_rate(1.0), Some(1_000_000));
        assert_eq!(ppm_from_rate(0.25), Some(250_000));
        assert_eq!(ppm_from_rate(0.000_001), Some(1));
        assert_eq!(ppm_from_rate(0.1), Some(100_000));
        for bad in [
            0.0,
            -0.5,
            1.000_001,
            2.0,
            f64::NAN,
            f64::INFINITY,
            0.123_456_7,
            0.999_999_9,
            0.000_000_5,
        ] {
            assert_eq!(ppm_from_rate(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_site_keeps_everything_until_told_otherwise_and_the_change_reaches_the_cache() {
        let (meta, site) = site();
        assert_eq!(
            meta.performance_sampling(site).expect("read"),
            SAMPLE_FULL_PPM
        );
        let before = meta.generation().expect("generation");
        meta.set_performance_sampling(site, 250_000).expect("set");
        assert_eq!(meta.performance_sampling(site).expect("read"), 250_000);
        assert!(meta.generation().expect("generation") > before);
        let snapshot = meta.snapshot().expect("snapshot");
        assert_eq!(snapshot.sampling, [(site, 250_000)]);
        meta.set_performance_sampling(site, SAMPLE_FULL_PPM)
            .expect("back");
        assert!(meta.snapshot().expect("snapshot").sampling.is_empty());
    }

    #[test]
    fn a_rate_out_of_range_or_a_missing_site_is_refused() {
        let (meta, site) = site();
        for ppm in [0, SAMPLE_FULL_PPM + 1] {
            assert!(matches!(
                meta.set_performance_sampling(site, ppm),
                Err(MetaError::Invalid(_))
            ));
        }
        assert!(matches!(
            meta.set_performance_sampling(site + 1, 10),
            Err(MetaError::NotFound("site"))
        ));
        assert!(matches!(
            meta.performance_sampling(site + 1),
            Err(MetaError::NotFound("site"))
        ));
        assert_eq!(
            meta.performance_sampling(site).expect("read"),
            SAMPLE_FULL_PPM
        );
    }
}
