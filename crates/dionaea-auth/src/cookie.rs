//! The session cookie: building `Set-Cookie` and reading `Cookie`.
//!
//! Only this one cookie is ever read, and the caller checks that its value
//! has the shape of a session secret before using it.

pub const COOKIE_NAME: &str = "dionaea_session";

/// `Set-Cookie` for a new session. `Secure` is left out only when the
/// operator says the server is reached over plain HTTP (local development).
pub fn set_cookie(secret: &str, max_age_secs: i64, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={secret}; Max-Age={max_age_secs}; Path=/; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

/// `Set-Cookie` that makes the browser forget the session.
pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE_NAME}=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

/// The value of the session cookie in a `Cookie` header, if there is exactly
/// one. Two cookies of that name are refused rather than guessed between, and
/// so is a header with a part that is not `name=value`.
pub fn from_header(header: &str) -> Option<&str> {
    let mut found = None;
    for pair in header.split(';') {
        let (name, value) = pair.split_once('=')?;
        if name.trim() == COOKIE_NAME {
            if found.is_some() {
                return None;
            }
            found = Some(value.trim());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_is_http_only_lax_and_secure_by_default() {
        let cookie = set_cookie("ds_abc", 60, true);
        for part in ["HttpOnly", "SameSite=Lax", "Secure", "Path=/", "Max-Age=60"] {
            assert!(cookie.contains(part), "{cookie}");
        }
        assert!(!set_cookie("ds_abc", 60, false).contains("Secure"));
        assert!(clear_cookie(true).contains("Max-Age=0"));
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        assert_eq!(from_header("a=1; dionaea_session=ds_x; b=2"), Some("ds_x"));
        assert_eq!(from_header("dionaea_session=ds_x"), Some("ds_x"));
        assert_eq!(from_header("a=1; b=2"), None);
        assert_eq!(from_header(""), None);
    }

    #[test]
    fn two_session_cookies_or_a_malformed_header_yield_nothing() {
        assert_eq!(from_header("dionaea_session=a; dionaea_session=b"), None);
        assert_eq!(from_header("dionaea_session=a; garbage"), None);
    }
}
