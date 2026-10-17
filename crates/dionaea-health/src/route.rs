//! Route templating: turns a path, or a URL, into a pattern that groups
//! requests instead of naming one.
//!
//! The query string, the fragment, matrix parameters and the scheme and host
//! are dropped; a segment that looks like an identifier becomes `:id`. A
//! segment already written as a template (`:id`, `{id}`, `[id]`) is kept, so
//! an SDK that knows the route pattern wins over the guess.

/// Most segments kept; the rest are cut.
pub const MAX_SEGMENTS: usize = 16;
/// Longest segment kept, in characters; a longer one becomes `:id`.
pub const MAX_SEGMENT_CHARS: usize = 48;
/// Longest template, in characters: what the guard allows a route to be.
pub use dionaea_core::MAX_ROUTE_CHARS;

/// The template of `route`. Always starts with `/`; an empty path is `/`.
pub fn template_route(route: &str) -> String {
    let path = path_of(route);
    let mut out = String::from("/");
    for (count, segment) in path
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        if count == MAX_SEGMENTS {
            break;
        }
        if count > 0 {
            out.push('/');
        }
        out.push_str(&template_segment(segment));
        if out.chars().count() >= MAX_ROUTE_CHARS {
            break;
        }
    }
    let cut: String = out.chars().take(MAX_ROUTE_CHARS).collect();
    cut.trim_end().to_owned()
}

/// The path of a URL or a path, without scheme, host, query, fragment or
/// matrix parameters.
fn path_of(route: &str) -> &str {
    let mut rest = route.trim();
    if let Some(at) = rest.find("://") {
        let scheme = &rest[..at];
        if !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic()) {
            rest = &rest[at + 3..];
            rest = match rest.find('/') {
                Some(slash) => &rest[slash..],
                None => "",
            };
        }
    }
    let end = rest.find(['?', '#', ';']).unwrap_or(rest.len());
    &rest[..end]
}

fn template_segment(segment: &str) -> String {
    if is_template(segment) {
        return segment.to_owned();
    }
    if looks_like_id(segment) {
        return ":id".to_owned();
    }
    segment.to_owned()
}

fn is_template(segment: &str) -> bool {
    let named = |inner: &str| {
        !inner.is_empty()
            && inner.len() <= 32
            && inner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    segment.strip_prefix(':').is_some_and(named)
        || segment
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .is_some_and(named)
        || segment
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .is_some_and(named)
}

fn looks_like_id(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    if chars.len() > MAX_SEGMENT_CHARS {
        return true;
    }
    if segment.contains('@') || segment.contains('%') {
        return true;
    }
    if chars.iter().all(char::is_ascii_digit) {
        return true;
    }
    if is_uuid(&chars) {
        return true;
    }
    let letter = chars.iter().any(char::is_ascii_alphabetic);
    let digit = chars.iter().any(char::is_ascii_digit);
    let hex = chars.iter().all(char::is_ascii_hexdigit);
    // Hexadecimal of 16 or more with a letter; a long token mixing letters and
    // digits; a short mix such as an order number (`A1234`) with 4 or more digits.
    (hex && chars.len() >= 16 && letter)
        || (chars.len() >= 20 && letter && digit)
        || (letter && chars.iter().filter(|c| c.is_ascii_digit()).count() >= 4)
        || segment.starts_with("eyJ")
}

fn is_uuid(chars: &[char]) -> bool {
    chars.len() == 36
        && chars.iter().enumerate().all(|(k, c)| match k {
            8 | 13 | 18 | 23 => *c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_become_placeholders_and_words_stay() {
        let cases = [
            ("/orders/12345/items", "/orders/:id/items"),
            ("/users/0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e", "/users/:id"),
            ("/files/9f86d081884c7d659a2feaa0c55ad015", "/files/:id"),
            ("/orders/A12345", "/orders/:id"),
            ("/v1/search", "/v1/search"),
            ("/api/v2/users", "/api/v2/users"),
            ("/profile/ana@example.com", "/profile/:id"),
            ("/", "/"),
            ("", "/"),
        ];
        for (input, expected) in cases {
            assert_eq!(template_route(input), expected, "{input}");
        }
    }

    #[test]
    fn urls_lose_scheme_host_query_fragment_and_matrix_parameters() {
        assert_eq!(
            template_route("https://api.example.com/v1/orders/77?token=abc#x"),
            "/v1/orders/:id"
        );
        assert_eq!(template_route("https://api.example.com"), "/");
        assert_eq!(template_route("/a/b;jsessionid=1/c?x=1"), "/a/b");
        assert_eq!(template_route("/a?b=1"), "/a");
    }

    #[test]
    fn whitespace_around_a_segment_does_not_hide_an_identifier() {
        // Found by the property test: a non-breaking space after the number
        // kept it from being recognised, and the template changed on a second pass.
        assert_eq!(template_route("0\u{a0};"), "/:id");
        assert_eq!(template_route("/orders/ 123 /items"), "/orders/:id/items");
    }

    #[test]
    fn a_template_the_sdk_sent_is_kept() {
        assert_eq!(
            template_route("/orders/:orderId/items/{item}"),
            "/orders/:orderId/items/{item}"
        );
        assert_eq!(template_route("/blog/[slug]"), "/blog/[slug]");
    }

    #[test]
    fn length_and_depth_are_bounded() {
        let deep = "/a".repeat(100);
        assert_eq!(template_route(&deep).matches('/').count(), MAX_SEGMENTS);
        let long = format!("/{}", "x".repeat(500));
        assert_eq!(template_route(&long), "/:id");
        let wide: String = (0..40)
            .map(|i| format!("/seg{}", "y".repeat(40 + i % 3)))
            .collect();
        assert!(template_route(&wide).chars().count() <= MAX_ROUTE_CHARS);
    }
}
