//! Whatever arrives, the rules do not panic, finish quickly, stay bounded and
//! give the same answer when applied to their own output.

use std::time::{Duration, Instant};

use dionaea_health::frames::{MAX_FRAMES, MAX_FUNCTION_CHARS, MAX_MODULE_CHARS};
use dionaea_health::route::MAX_ROUTE_CHARS;
use dionaea_health::{normalize_frames, scrub_message, template_route};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn scrubbing_is_idempotent_and_never_longer_than_a_constant_factor(text in "\\PC{0,400}") {
        let once = scrub_message(&text);
        prop_assert_eq!(scrub_message(&once), once.clone());
        // The longest mask, `[userinfo]`, replaces at least a character and
        // the longest insertion is `?[query]` for a lone `?`.
        prop_assert!(once.chars().count() <= text.chars().count() * 9 + 16);
    }

    #[test]
    fn scrubbing_text_made_of_the_interesting_characters(
        text in proptest::collection::vec(
            prop_oneof![
                Just('@'), Just('.'), Just('/'), Just(':'), Just('?'), Just('#'),
                Just('-'), Just('_'), Just(' '), Just('1'), Just('a'), Just('e'),
                Just('J'), Just('0'), Just('f'),
            ],
            0..300,
        )
    ) {
        let text: String = text.into_iter().collect();
        let once = scrub_message(&text);
        prop_assert_eq!(scrub_message(&once), once);
    }

    #[test]
    fn templates_are_idempotent_and_bounded(text in "\\PC{0,400}") {
        let once = template_route(&text);
        prop_assert!(once.starts_with('/'));
        prop_assert!(once.chars().count() <= MAX_ROUTE_CHARS);
        prop_assert_eq!(template_route(&once), once);
    }

    #[test]
    fn normalized_frames_are_idempotent_and_bounded(text in "[a-z|;%0-9 ]{0,600}") {
        let once = normalize_frames(&text);
        prop_assert_eq!(normalize_frames(&once), once.clone());
        prop_assert!(once.split(';').count() <= MAX_FRAMES);
        let longest = MAX_FRAMES * (MAX_MODULE_CHARS + MAX_FUNCTION_CHARS) * 3 + 64 * 40;
        prop_assert!(once.len() <= longest);
    }
}

/// Inputs built to make a careless matcher quadratic finish in linear time.
#[test]
fn adversarial_inputs_finish_quickly() {
    let big = 200_000;
    let inputs = [
        "@".repeat(big),
        "a@".repeat(big / 2),
        "a".repeat(big),
        "1 ".repeat(big / 2),
        "1-".repeat(big / 2),
        "https://".repeat(big / 8),
        "?#".repeat(big / 2),
        format!("{}@a.b", "a".repeat(big)),
        "eyJ.".repeat(big / 4),
        "/".repeat(big),
        "/a".repeat(big / 2),
        "|;".repeat(big / 2),
    ];
    for input in &inputs {
        let started = Instant::now();
        let _ = scrub_message(input);
        let _ = template_route(input);
        let _ = normalize_frames(input);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?} on an input starting {:?}",
            started.elapsed(),
            &input[..input.len().min(12)]
        );
    }
}
