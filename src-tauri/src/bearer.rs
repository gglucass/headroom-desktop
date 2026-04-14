use std::time::{Duration, Instant};

/// How long a captured bearer token is considered usable before it must be
/// re-captured from a fresh request. Claude Code's OAuth access tokens rotate
/// roughly hourly; a conservative TTL keeps stale tokens from being reused
/// against the OAuth endpoints.
pub const BEARER_TOKEN_TTL: Duration = Duration::from_secs(60 * 60);

/// Bearer token captured from a pass-through HTTP request.
///
/// The inner string is not exposed by `Debug`, `Display`, or serde. Callers
/// must go through `value_if_fresh` to access the secret, which also enforces
/// the TTL.
#[derive(Clone)]
pub struct BearerToken {
    value: String,
    captured_at: Instant,
}

impl BearerToken {
    pub fn new(value: String) -> Self {
        Self {
            value,
            captured_at: Instant::now(),
        }
    }

    pub fn value_if_fresh(&self, ttl: Duration) -> Option<&str> {
        if self.captured_at.elapsed() < ttl {
            Some(&self.value)
        } else {
            None
        }
    }

    pub fn age(&self) -> Duration {
        self.captured_at.elapsed()
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BearerToken(redacted, age={:?})", self.age())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_token_returns_value() {
        let t = BearerToken::new("secret-abc".into());
        assert_eq!(t.value_if_fresh(Duration::from_secs(60)), Some("secret-abc"));
    }

    #[test]
    fn expired_token_returns_none() {
        let t = BearerToken {
            value: "secret-abc".into(),
            captured_at: Instant::now() - Duration::from_secs(120),
        };
        assert!(t.value_if_fresh(Duration::from_secs(60)).is_none());
    }

    #[test]
    fn debug_output_never_leaks_value() {
        let t = BearerToken::new("sk-ant-hypothetical-secret".into());
        let rendered = format!("{t:?}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("sk-ant"));
        assert!(rendered.contains("redacted"));
    }
}
