use std::fmt;

use serde::{Deserialize, Serialize};

/// Opaque wrapper for sensitive strings (tokens, passwords, API keys).
///
/// `Debug` output is redacted to `Secret("***")` to prevent accidental
/// leaking in logs, tracing output, or error messages. There is no
/// `Display` impl — callers must explicitly call [`Secret::expose`] to
/// access the inner value.
///
/// `#[serde(transparent)]` keeps the JSON wire format a bare string.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Create a new `Secret` from any string-like value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Expose the inner secret value.
    ///
    /// Use this intentionally — e.g. for sending in an HTTP header or
    /// comparing against an incoming credential.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(\"***\")")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Secret {}

impl PartialEq<str> for Secret {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Secret {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("super-secret-token");
        assert_eq!(format!("{:?}", s), r#"Secret("***")"#);
    }

    #[test]
    fn expose_returns_value() {
        let s = Secret::new("my-token");
        assert_eq!(s.expose(), "my-token");
    }

    #[test]
    fn serde_roundtrip() {
        let s = Secret::new("abc123");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""abc123""#);
        let back: Secret = serde_json::from_str(&json).unwrap();
        assert_eq!(back.expose(), "abc123");
    }

    #[test]
    fn partial_eq_str() {
        let s = Secret::new("token");
        assert!(s == "token");
        assert!(s != "other");
    }
}
