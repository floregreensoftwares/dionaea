use std::fmt;

/// Longest event, field and enum-value name, in characters.
pub const MAX_NAME_LEN: usize = 64;

/// A lowercase identifier: `[a-z][a-z0-9_]*`, at most [`MAX_NAME_LEN`] long.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidName;

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "names are lowercase identifiers matching [a-z][a-z0-9_]* with at most {MAX_NAME_LEN} characters"
        )
    }
}

impl std::error::Error for InvalidName {}

impl Name {
    pub fn new(value: &str) -> Result<Self, InvalidName> {
        let mut chars = value.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if first_ok && rest_ok && value.len() <= MAX_NAME_LEN {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidName)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_lowercase_identifiers() {
        for value in ["a", "checkout_started", "step2", "a_1_b"] {
            assert!(Name::new(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn rejects_everything_else() {
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        for value in [
            "", "1a", "_a", "A", "a-b", "a b", "a.b", "é", "a\n", &too_long,
        ] {
            assert!(Name::new(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn accepts_the_longest_name() {
        assert!(Name::new(&"a".repeat(MAX_NAME_LEN)).is_ok());
    }
}
