//! Calendar arithmetic for the timestamps ClickHouse is given, so the store
//! needs no date library. Days are counted from the Unix epoch in the
//! proleptic Gregorian calendar (the algorithm of H. Hinnant's `civil_from_days`).

/// The last millisecond ClickHouse's `DateTime64` holds (the end of 2299).
pub(crate) const MAX_MS: u64 = 10_413_791_999_999;

/// Year, month and day of a count of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `YYYY-MM-DD` for a millisecond timestamp.
pub(crate) fn format_date(ms: u64) -> String {
    let (year, month, day) = civil_from_days((ms / 86_400_000) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `YYYY-MM-DD hh:mm:ss.mmm` (UTC) for a millisecond timestamp.
pub(crate) fn format_datetime(ms: u64) -> String {
    let in_day = ms % 86_400_000;
    format!(
        "{} {:02}:{:02}:{:02}.{:03}",
        format_date(ms),
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_are_formatted_correctly() {
        for (ms, expected) in [
            (0, "1970-01-01 00:00:00.000"),
            (1, "1970-01-01 00:00:00.001"),
            (86_399_999, "1970-01-01 23:59:59.999"),
            (86_400_000, "1970-01-02 00:00:00.000"),
            (951_782_400_000, "2000-02-29 00:00:00.000"),
            (1_760_000_000_000, "2025-10-09 08:53:20.000"),
            (1_781_222_400_123, "2026-06-12 00:00:00.123"),
            (4_102_444_800_000, "2100-01-01 00:00:00.000"),
            (MAX_MS, "2299-12-31 23:59:59.999"),
        ] {
            assert_eq!(format_datetime(ms), expected, "{ms}");
        }
    }

    #[test]
    fn leap_days_and_year_boundaries_are_right() {
        assert_eq!(format_date(1_709_164_800_000), "2024-02-29");
        assert_eq!(format_date(1_709_251_200_000), "2024-03-01");
        assert_eq!(format_date(1_735_689_599_000), "2024-12-31");
        assert_eq!(format_date(1_735_689_600_000), "2025-01-01");
        // 2100 is not a leap year: the day after 28 February is 1 March.
        assert_eq!(format_date(4_107_456_000_000), "2100-02-28");
        assert_eq!(format_date(4_107_542_400_000), "2100-03-01");
    }

    #[test]
    fn every_day_over_four_centuries_matches_a_running_count() {
        let mut expected = (1970, 1u32, 1u32);
        let month_days = |year: i64, month: u32| match month {
            2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        for day in 0..146_097u64 {
            let text = format_date(day * 86_400_000);
            assert_eq!(
                text,
                format!("{:04}-{:02}-{:02}", expected.0, expected.1, expected.2),
                "day {day}"
            );
            expected.2 += 1;
            if expected.2 > month_days(expected.0, expected.1) {
                expected = if expected.1 == 12 {
                    (expected.0 + 1, 1, 1)
                } else {
                    (expected.0, expected.1 + 1, 1)
                };
            }
        }
    }
}
