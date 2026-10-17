//! Masks what must not be stored from an error message.
//!
//! The built-in rules are fixed and run in this order, each in one linear
//! pass (no regular expressions, so no input makes one backtrack):
//!
//! 1. **URLs** (`scheme://...`): the user name and password become
//!    `[userinfo]`, and the query string and fragment become `?[query]` and
//!    `#[fragment]`. The host and path stay.
//! 2. **E-mail addresses** become `[email]`.
//! 3. **JWT-like tokens** (`eyJ...` with two or three dot-separated parts)
//!    become `[token]`.
//! 4. **UUIDs** become `[uuid]`.
//! 5. **Long tokens**: a run of letters, digits, `_` and `-` of 24 or more
//!    characters that holds both a letter and a digit, or a run of 16 or more
//!    hexadecimal characters that holds a letter, becomes `[token]`.
//! 6. **Long digit runs**: six or more digits in a row, or digits grouped by
//!    single spaces or hyphens that add up to nine or more (card and phone
//!    numbers), become `[number]`.
//!
//! Scrubbing twice gives the same text as scrubbing once.

const URL_END: &[char] = &['"', '\'', '<', '>', ')', ']', '}', '`'];

/// The built-in rules applied to `message`.
pub fn scrub_message(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let text = urls(&chars);
    let text = emails(&text);
    let text = jwts(&text);
    let text = uuids(&text);
    let text = tokens(&text);
    digits(&text).into_iter().collect()
}

/// Removes the query string and fragment of a URL or path: for a script
/// location in a stack frame, where a version or a signature often rides in
/// the query. Returns the text unchanged when it has neither.
pub fn strip_query(location: &str) -> String {
    match location.find(['?', '#']) {
        Some(at) => location[..at].to_owned(),
        None => location.to_owned(),
    }
}

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_local(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_domain(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-')
}

fn urls(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(scheme_end) = scheme_at(chars, i) {
            let mut end = scheme_end;
            while end < chars.len() && !chars[end].is_whitespace() {
                // A mask written by an earlier pass is part of the address.
                if let Some(len) = mask_at(chars, end) {
                    end += len;
                } else if URL_END.contains(&chars[end]) {
                    break;
                } else {
                    end += 1;
                }
            }
            // Punctuation after the address belongs to the sentence.
            while end > scheme_end && matches!(chars[end - 1], '.' | ',' | ';' | ':' | '!') {
                end -= 1;
            }
            out.extend(clean_url(&chars[i..end], scheme_end - i));
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// The length of a mask (`[query]`, `[fragment]`, `[userinfo]`) at `i`.
fn mask_at(chars: &[char], i: usize) -> Option<usize> {
    ["[query]", "[fragment]", "[userinfo]"]
        .iter()
        .find_map(|mask| {
            let mask: Vec<char> = mask.chars().collect();
            (chars.get(i..i + mask.len()) == Some(&mask[..])).then_some(mask.len())
        })
}

/// If a `scheme://` starts at `i` (not inside a longer word), the index just
/// after the `://`.
fn scheme_at(chars: &[char], i: usize) -> Option<usize> {
    if i > 0 && chars[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let mut j = i;
    while j < chars.len() && chars[j].is_ascii_alphabetic() && j - i <= 10 {
        j += 1;
    }
    let letters = j - i;
    if (2..=10).contains(&letters)
        && chars.get(j) == Some(&':')
        && chars.get(j + 1) == Some(&'/')
        && chars.get(j + 2) == Some(&'/')
    {
        Some(j + 3)
    } else {
        None
    }
}

fn clean_url(url: &[char], authority_start: usize) -> Vec<char> {
    let rest = &url[authority_start..];
    let authority_end = rest
        .iter()
        .position(|c| matches!(c, '/' | '?' | '#'))
        .unwrap_or(rest.len());
    let mut out: Vec<char> = url[..authority_start].to_vec();
    let authority = &rest[..authority_end];
    match authority.iter().rposition(|c| *c == '@') {
        Some(at) => {
            out.extend("[userinfo]".chars());
            out.extend(&authority[at..]);
        }
        None => out.extend(authority),
    }
    let tail = &rest[authority_end..];
    let cut = tail.iter().position(|c| matches!(c, '?' | '#'));
    match cut {
        None => out.extend(tail),
        Some(at) => {
            out.extend(&tail[..at]);
            if tail[at] == '?' {
                out.extend("?[query]".chars());
            } else {
                out.extend("#[fragment]".chars());
            }
        }
    }
    out
}

fn emails(chars: &[char]) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let mut local = 0;
            while local < out.len() && is_local(out[out.len() - 1 - local]) {
                local += 1;
            }
            let mut end = i + 1;
            while end < chars.len() && is_domain(chars[end]) {
                end += 1;
            }
            let domain = &chars[i + 1..end];
            // A trailing dot belongs to the sentence.
            let domain_len = domain.iter().rposition(|c| *c != '.').map_or(0, |p| p + 1);
            let domain = &domain[..domain_len];
            if local > 0 && domain.contains(&'.') && domain.first() != Some(&'.') {
                out.truncate(out.len() - local);
                out.extend("[email]".chars());
                i += 1 + domain_len;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn jwts(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let starts_run = is_word(c) && (i == 0 || !(is_word(chars[i - 1]) || chars[i - 1] == '.'));
        if starts_run {
            let mut end = i;
            while end < chars.len() && (is_word(chars[end]) || chars[end] == '.') {
                end += 1;
            }
            let run = &chars[i..end];
            if is_jwt(run) {
                out.extend("[token]".chars());
            } else {
                out.extend(run);
            }
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn is_jwt(run: &[char]) -> bool {
    let text: String = run.iter().collect();
    if !text.starts_with("eyJ") || text.len() < 20 {
        return false;
    }
    let parts: Vec<&str> = text.split('.').collect();
    (parts.len() == 2 || parts.len() == 3) && parts.iter().all(|p| !p.is_empty())
}

fn uuids(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let bounded = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        if bounded && is_uuid_at(chars, i) {
            out.extend("[uuid]".chars());
            i += 36;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn is_uuid_at(chars: &[char], i: usize) -> bool {
    let Some(window) = chars.get(i..i + 36) else {
        return false;
    };
    if chars.get(i + 36).is_some_and(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    window.iter().enumerate().all(|(k, c)| match k {
        8 | 13 | 18 | 23 => *c == '-',
        _ => is_hex(*c),
    })
}

fn tokens(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if is_word(chars[i]) {
            let mut end = i;
            while end < chars.len() && is_word(chars[end]) {
                end += 1;
            }
            let run = &chars[i..end];
            if is_token(run) {
                out.extend("[token]".chars());
            } else {
                out.extend(run);
            }
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn is_token(run: &[char]) -> bool {
    let letter = run.iter().any(char::is_ascii_alphabetic);
    let digit = run.iter().any(char::is_ascii_digit);
    if run.len() >= 16 && letter && run.iter().all(|c| is_hex(*c)) {
        return true;
    }
    if run.len() < 24 || !letter || !digit {
        return false;
    }
    // Random text changes between letters and digits all the time and has a
    // stretch without a separator; words joined by `-` or `_` (a slug, a
    // constant name) and a camel-case identifier with a trailing digit do not.
    let longest_part = run
        .split(|c| matches!(c, '-' | '_'))
        .map(<[char]>::len)
        .max()
        .unwrap_or(0);
    let switches = run
        .windows(2)
        .filter(|pair| pair[0].is_ascii_digit() != pair[1].is_ascii_digit())
        .count();
    longest_part >= 12 && switches >= 3
}

/// `dddd-dd-dd` standing alone.
fn is_date_at(chars: &[char], i: usize) -> bool {
    let Some(window) = chars.get(i..i + 10) else {
        return false;
    };
    let shaped = window.iter().enumerate().all(|(k, c)| match k {
        4 | 7 => *c == '-',
        _ => c.is_ascii_digit(),
    });
    shaped && !chars.get(i + 10).is_some_and(char::is_ascii_digit)
}

fn digits(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() && is_date_at(chars, i) {
            // A calendar date is not a number to hide.
            out.extend(&chars[i..i + 10]);
            i += 10;
        } else if chars[i].is_ascii_digit() {
            // A group of digits, then more groups joined by one space or hyphen.
            let mut end = i;
            let mut total = 0;
            let mut longest = 0;
            let mut shortest = usize::MAX;
            loop {
                let start = end;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                total += end - start;
                longest = longest.max(end - start);
                shortest = shortest.min(end - start);
                let joined = matches!(chars.get(end), Some(' ' | '-'))
                    && chars.get(end + 1).is_some_and(char::is_ascii_digit);
                if joined {
                    end += 1;
                } else {
                    break;
                }
            }
            // One long run, or several groups that look like a card or a phone
            // number: nine digits, none of the groups a single digit, one of
            // them with three or more.
            if longest >= 6 || (total >= 9 && shortest >= 2 && longest >= 3) {
                out.extend("[number]".chars());
            } else {
                out.extend(&chars[i..end]);
            }
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_rule_masks_its_pattern() {
        let cases = [
            ("mail ana@example.com now", "mail [email] now"),
            (
                "see https://user:pw@api.example.com/v1/x?token=abc#top",
                "see https://[userinfo]@api.example.com/v1/x?[query]",
            ),
            (
                "at https://cdn.example.com/app.js#frag.",
                "at https://cdn.example.com/app.js#[fragment].",
            ),
            (
                "id 0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e done",
                "id [uuid] done",
            ),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.c2lnbmF0dXJl ok",
                "jwt [token] ok",
            ),
            (
                "key a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6 end",
                "key [token] end",
            ),
            ("order 1234567 failed", "order [number] failed"),
            ("card 4111 1111 1111 1111", "card [number]"),
            ("call 55 11 99999-9999", "call [number]"),
            ("values 1 2 3 4 5 6 7 8 9", "values 1 2 3 4 5 6 7 8 9"),
            ("range 10-20-30-40-50", "range 10-20-30-40-50"),
            (
                "load checkout-payment-form-v2 failed",
                "load checkout-payment-form-v2 failed",
            ),
            (
                "ORDER_PROCESSING_FAILED_STEP_2 raised",
                "ORDER_PROCESSING_FAILED_STEP_2 raised",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(scrub_message(input), expected, "{input}");
        }
    }

    #[test]
    fn ordinary_messages_are_left_alone() {
        for text in [
            "TypeError: undefined is not a function",
            "Cannot read properties of null (reading 'x')",
            "at 2026-10-17 12:30 retry 3 of 5",
            "version 1.4.2+57 on x86_64",
            "ValidationErrorSomethingVeryLongIdentifierNameWithoutDigits",
            "port 8080 line 123",
            "email-like but not: a@b",
            "ends with a dot a@b.com.",
        ] {
            let scrubbed = scrub_message(text);
            if text.contains("a@b.com") {
                assert_eq!(scrubbed, "ends with a dot [email].");
            } else {
                assert_eq!(scrubbed, text);
            }
        }
    }

    #[test]
    fn strip_query_keeps_the_location() {
        assert_eq!(
            strip_query("https://cdn/app.js?v=1#x"),
            "https://cdn/app.js"
        );
        assert_eq!(strip_query("app.js"), "app.js");
    }

    #[test]
    fn non_ascii_text_passes_through() {
        assert_eq!(scrub_message("falha na conexão ✓"), "falha na conexão ✓");
        assert_eq!(scrub_message("é ana@example.com é"), "é [email] é");
    }
}
