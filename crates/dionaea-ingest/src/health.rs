//! What the server does to the health events after the guard has accepted
//! their shape: masks what must not be stored of an error message, keeps a
//! stack to code locations, and turns a route into a template. Only the
//! built-in health events are touched; a site cannot declare these names.

use dionaea_core::MAX_MESSAGE_CHARS;
use dionaea_health::{normalize_frames, scrub_message, strip_query, template_route};

/// The screen of an event without a query string or fragment and, for a path
/// (`/orders/123456`), with identifier-like segments as `:id`. A screen name
/// that is not a path (`Home`) is only stripped of a query.
pub(crate) fn screen(screen: &str) -> String {
    if screen.starts_with('/') {
        template_route(screen)
    } else {
        strip_query(screen)
    }
}

/// A message the guard may have cut at its maximum length ends in a word that
/// may be the front of an address, a token or a number the masks would no
/// longer recognise; that last word is dropped.
fn without_cut_tail(message: &str) -> &str {
    if message.chars().count() < MAX_MESSAGE_CHARS {
        return message;
    }
    message
        .rfind(char::is_whitespace)
        .map_or("", |at| message[..at].trim_end())
}

/// Applies the health rules to the text fields of an accepted event.
pub(crate) fn sanitize(event: &str, strings: &mut Vec<(String, String)>) {
    match event {
        "app_error" => {
            for (name, value) in strings.iter_mut() {
                match name.as_str() {
                    "message" => *value = scrub_message(without_cut_tail(value)),
                    "frames" => *value = normalize_frames(value),
                    _ => {}
                }
            }
            // A stack with no readable frame is no stack.
            strings.retain(|(name, value)| {
                !((name == "frames" || name == "message") && value.is_empty())
            });
        }
        "http_request" | "web_vital" => {
            for (name, value) in strings.iter_mut() {
                if name == "route" {
                    *value = template_route(value);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn an_error_message_is_scrubbed_and_its_stack_normalized() {
        let mut found = fields(&[
            ("kind", "unhandled"),
            ("type", "TypeError"),
            ("message", "no account for ana@example.com"),
            (
                "frames",
                "https://cdn.example.com/app.js?v=3|f|1|2|1;broken",
            ),
        ]);
        sanitize("app_error", &mut found);
        assert_eq!(
            found,
            fields(&[
                ("kind", "unhandled"),
                ("type", "TypeError"),
                ("message", "no account for [email]"),
                ("frames", "https://cdn.example.com/app.js|f|1|2|1"),
            ])
        );
    }

    #[test]
    fn a_message_cut_at_the_maximum_loses_its_last_word() {
        let body = "word ".repeat(101);
        let cut = format!("{body}ana@exampl");
        let cut: String = cut.chars().take(MAX_MESSAGE_CHARS).collect();
        let mut found = fields(&[("message", cut.as_str())]);
        sanitize("app_error", &mut found);
        assert!(!found[0].1.contains("ana"), "{}", found[0].1);
        assert!(found[0].1.starts_with("word word"));

        // One short of the maximum was not cut.
        let kept: String = format!("{body}ana@exampl")
            .chars()
            .take(MAX_MESSAGE_CHARS - 1)
            .collect();
        let mut found = fields(&[("message", kept.as_str())]);
        sanitize("app_error", &mut found);
        assert_eq!(found[0].1, scrub_message(&kept));

        // A message that is one long word has nothing safe to keep.
        let one_word = "a".repeat(MAX_MESSAGE_CHARS);
        let mut found = fields(&[("kind", "crash"), ("message", one_word.as_str())]);
        sanitize("app_error", &mut found);
        assert_eq!(found, fields(&[("kind", "crash")]));
    }

    #[test]
    fn a_screen_loses_its_query_and_its_ids() {
        assert_eq!(screen("/orders/123456?token=abc"), "/orders/:id");
        assert_eq!(screen("/orders/:id"), "/orders/:id");
        assert_eq!(screen("Home"), "Home");
        assert_eq!(screen("Home?x=1"), "Home");
        assert_eq!(screen("/"), "/");
    }

    #[test]
    fn a_stack_with_nothing_readable_is_left_out() {
        let mut found = fields(&[("kind", "crash"), ("frames", "garbage")]);
        sanitize("app_error", &mut found);
        assert_eq!(found, fields(&[("kind", "crash")]));
    }

    #[test]
    fn routes_become_templates_and_other_events_are_untouched() {
        let mut call = fields(&[
            ("route", "/orders/12345?token=abc"),
            ("host", "api.example.com"),
        ]);
        sanitize("http_request", &mut call);
        assert_eq!(
            call,
            fields(&[("route", "/orders/:id"), ("host", "api.example.com")])
        );
        let mut vital = fields(&[("route", "https://x.example.com/p/99")]);
        sanitize("web_vital", &mut vital);
        assert_eq!(vital, fields(&[("route", "/p/:id")]));

        let mut other = fields(&[("message", "ana@example.com"), ("route", "/orders/12345")]);
        sanitize("checkout", &mut other);
        assert_eq!(
            other,
            fields(&[("message", "ana@example.com"), ("route", "/orders/12345")])
        );
    }
}
