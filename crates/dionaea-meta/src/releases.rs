//! The order of two release labels, when the labels themselves say it.
//!
//! A label such as `1.4.2`, `v2.0`, `1.4.2+57` or `3.0.0-rc.1` is a version:
//! dotted numbers (missing trailing numbers are zero), an optional
//! pre-release after `-` (it starts with a letter, as `rc.1`; a date such as
//! `2026-10-18` is not a version) that sorts before the release itself, and an
//! optional numeric build after `+` that breaks a tie. Any other label (a
//! commit hash, a date, a name) is not a version and has no order of its own;
//! the caller then falls back to when each release was first seen.

use std::cmp::Ordering;

/// Most numbers in the dotted part; more is not a version.
const MAX_PARTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    parts: Vec<u64>,
    /// `None` for a release; the text after `-` for a pre-release.
    pre: Option<String>,
    build: u64,
}

fn parse(label: &str) -> Option<Version> {
    let label = label.strip_prefix(['v', 'V']).unwrap_or(label);
    let (rest, build) = match label.split_once('+') {
        Some((rest, build)) => (rest, build.parse::<u64>().ok()?),
        None => (label, 0),
    };
    let (numbers, pre) = match rest.split_once('-') {
        Some((numbers, pre)) if pre.starts_with(|c: char| c.is_ascii_alphabetic()) => {
            (numbers, Some(pre.to_owned()))
        }
        Some(_) => return None,
        None => (rest, None),
    };
    let mut parts = Vec::new();
    for part in numbers.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(part.parse::<u64>().ok()?);
    }
    if parts.len() > MAX_PARTS {
        return None;
    }
    while parts.len() > 1 && parts.last() == Some(&0) {
        parts.pop();
    }
    Some(Version { parts, pre, build })
}

/// Pre-release identifiers as semver orders them: dot-separated, numbers by
/// value and before words, words by text, a prefix before what extends it.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        let order = match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(m), Ok(n)) => m.cmp(&n),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            },
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

/// How `a` compares with `b` when both are versions; `None` when either is
/// not, and the order is not known from the labels.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (parse(a)?, parse(b)?);
    Some(
        a.parts
            .cmp(&b.parts)
            .then_with(|| match (&a.pre, &b.pre) {
                (None, None) => Ordering::Equal,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(x), Some(y)) => compare_pre(x, y),
            })
            .then_with(|| a.build.cmp(&b.build)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &str, b: &str) -> Option<Ordering> {
        compare_versions(a, b)
    }

    #[test]
    fn numbers_compare_as_numbers_not_as_text() {
        assert_eq!(cmp("1.10.0", "1.9.0"), Some(Ordering::Greater));
        assert_eq!(cmp("2.0", "10.0"), Some(Ordering::Less));
        assert_eq!(cmp("1.2", "1.2.0"), Some(Ordering::Equal));
        assert_eq!(cmp("v1.2.0", "1.2"), Some(Ordering::Equal));
        assert_eq!(cmp("1.2.1", "1.2"), Some(Ordering::Greater));
    }

    #[test]
    fn a_pre_release_is_before_its_release_and_a_build_breaks_ties() {
        assert_eq!(cmp("3.0.0-rc.1", "3.0.0"), Some(Ordering::Less));
        assert_eq!(cmp("3.0.0-rc.1", "3.0.0-rc.2"), Some(Ordering::Less));
        assert_eq!(cmp("3.0.0-rc.2", "3.0.0-rc.10"), Some(Ordering::Less));
        assert_eq!(cmp("3.0.0-rc", "3.0.0-rc.1"), Some(Ordering::Less));
        assert_eq!(
            cmp("3.0.0-alpha.1", "3.0.0-alpha.beta"),
            Some(Ordering::Less)
        );
        assert_eq!(cmp("3.0.0-rc.1", "2.9.9"), Some(Ordering::Greater));
        assert_eq!(cmp("1.4.2+57", "1.4.2+58"), Some(Ordering::Less));
        assert_eq!(cmp("1.4.2+100", "1.4.2+99"), Some(Ordering::Greater));
        assert_eq!(cmp("1.4.3+1", "1.4.2+99"), Some(Ordering::Greater));
        assert_eq!(cmp("1.4.2", "1.4.2+0"), Some(Ordering::Equal));
    }

    #[test]
    fn a_label_that_is_not_a_version_has_no_order() {
        for label in [
            "abc123def",
            "2026-10-18",
            "1..2",
            "1.2.x",
            "1.2.3+beta",
            "1.2.3-",
            "",
            "1.2.3.4.5.6.7.8.9",
            "99999999999999999999999.1",
        ] {
            assert_eq!(cmp(label, "1.0.0"), None, "{label}");
            assert_eq!(cmp("1.0.0", label), None, "{label}");
        }
    }

    #[test]
    fn the_order_is_total_and_antisymmetric_among_versions() {
        let labels = [
            "0.9",
            "1.0.0-alpha",
            "1.0.0-beta",
            "1.0.0",
            "1.0.0+3",
            "1.0.1",
            "1.10",
            "2",
            "v3.0.0-rc.1",
            "3.0.0",
        ];
        for a in labels {
            assert_eq!(cmp(a, a), Some(Ordering::Equal));
            for b in labels {
                assert_eq!(
                    cmp(a, b).map(Ordering::reverse),
                    cmp(b, a),
                    "{a} against {b}"
                );
            }
        }
        let mut sorted = labels.to_vec();
        sorted.sort_by(|a, b| cmp(a, b).expect("versions"));
        assert_eq!(sorted, labels);
    }
}
