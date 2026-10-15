use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

/// Longest event, field and enum-value name, in characters.
pub const MAX_NAME_LEN: usize = 64;

/// A lowercase identifier: `[a-z][a-z0-9_]*`, at most [`MAX_NAME_LEN`] long.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Arc<str>);

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
        if Self::is_valid(value) {
            Ok(Self(value.into()))
        } else {
            Err(InvalidName)
        }
    }

    /// The same check as [`Name::new`], without allocating.
    pub fn is_valid(value: &str) -> bool {
        if value.len() > MAX_NAME_LEN {
            return false;
        }
        let mut chars = value.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        first_ok && rest_ok
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lets maps keyed by `Name` be searched with a plain `&str`.
impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
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

/// A declared `enum` value: lowercase, `[a-z0-9][a-z0-9_.-]*`, at most
/// [`MAX_NAME_LEN`] long. Wider than [`Name`] so values like `us-east`, `3d`
/// or `v1.2` can be declared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumValue(Arc<str>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidEnumValue;

impl fmt::Display for InvalidEnumValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "enum values match [a-z0-9][a-z0-9_.-]* with at most {MAX_NAME_LEN} characters"
        )
    }
}

impl std::error::Error for InvalidEnumValue {}

impl EnumValue {
    pub fn new(value: &str) -> Result<Self, InvalidEnumValue> {
        if Self::is_valid(value) {
            Ok(Self(value.into()))
        } else {
            Err(InvalidEnumValue)
        }
    }

    pub fn is_valid(value: &str) -> bool {
        if value.len() > MAX_NAME_LEN {
            return false;
        }
        let mut chars = value.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_ok = chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'));
        first_ok && rest_ok
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for EnumValue {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod enum_value_tests {
    use super::*;

    #[test]
    fn accepts_hyphens_dots_and_leading_digits() {
        for value in ["a", "us-east", "3d", "v1.2", "a_b", "0"] {
            assert!(EnumValue::new(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn rejects_uppercase_symbols_whitespace_and_leading_separators() {
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        for value in [
            "", "A", "en-US", "-a", ".a", "_a", "a b", "a/b", "é", &too_long,
        ] {
            assert!(EnumValue::new(value).is_err(), "{value:?}");
        }
    }
}
