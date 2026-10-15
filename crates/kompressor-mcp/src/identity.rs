//! Where the tenant comes from, and where it must never come from.
//!
//! Everything durable on this surface is scoped by tenant: a retained fragment
//! is keyed by it, and so is every statistic. A tool argument is written by the
//! model, and the model is the least trustworthy participant in the exchange —
//! it can be steered by the very payload it is compressing. So the tenant is
//! resolved from the connection before any tool code runs, and the tool layer
//! has no way to reach a tenant that was not resolved here. A `tenant` field in
//! a tool's arguments is not rejected; it is simply not a thing the tool can
//! read.

use http::HeaderMap;
use kompressor_core::auth::{Authenticator, Credentials, Identity};
use kompressor_core::error::AuthError;

/// `Authorization: Bearer <key>`.
const AUTHORIZATION: &str = "authorization";
/// The spelling the OpenAI-style clients use.
const X_API_KEY: &str = "x-api-key";
/// The spelling this service documents.
const X_KOMPRESSOR_KEY: &str = "x-kompressor-key";
/// The trusted internal caller's credential.
const X_KOMPRESSOR_SECRET: &str = "x-kompressor-secret";
/// The tenant a trusted caller is acting for. Honoured only with the secret.
const X_KOMPRESSOR_TENANT: &str = "x-kompressor-tenant";

/// Read the credentials a request presented.
///
/// Both `x-api-key` and `x-kompressor-key` are accepted, not just the bearer
/// form. A client authenticating the way the Anthropic SDK does presents
/// `x-api-key` and nothing else; recognising only `Authorization` would leave
/// that traffic unauthenticated and therefore silently never compressed.
pub fn credentials_from(headers: &HeaderMap) -> Credentials<'_> {
    let get = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    Credentials {
        bearer: get(AUTHORIZATION).and_then(strip_bearer),
        api_key: get(X_API_KEY),
        kompressor_key: get(X_KOMPRESSOR_KEY),
        secret: get(X_KOMPRESSOR_SECRET),
        requested_tenant: get(X_KOMPRESSOR_TENANT),
    }
}

/// Take the key out of an `Authorization` value, accepting any case of the
/// scheme.
///
/// A value with no scheme is treated as the key itself, since a client that
/// sends a bare token is trying to authenticate rather than to send a keyword —
/// with the one exception of the scheme word alone, which is a truncated header
/// and not a credential anyone configured.
fn strip_bearer(value: &str) -> Option<&str> {
    let value = value.trim();
    match value.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => {
            Some(rest.trim()).filter(|rest| !rest.is_empty())
        }
        Some(_) => None,
        None => {
            Some(value).filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("bearer"))
        }
    }
}

/// Resolve a request's headers into an identity.
pub fn resolve(auth: &Authenticator, headers: &HeaderMap) -> Result<Identity, AuthError> {
    auth.resolve(&credentials_from(headers))
}

/// How a handler learns which tenant it is acting for.
#[derive(Clone, Debug)]
pub enum IdentitySource {
    /// Streamable HTTP: the identity was resolved from the request's headers at
    /// the edge and travels in the request extensions. A handler that cannot
    /// find it there refuses rather than guessing, because guessing would mean
    /// serving one tenant's fragments to another.
    Connection,
    /// stdio: there is no connection to read, so the identity is fixed for the
    /// life of the process and comes from whatever launched it.
    Fixed(Box<Identity>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use kompressor_core::config::{ApiKey, AuthConfig, AuthMode};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn keyed() -> Authenticator {
        Authenticator::new(AuthConfig {
            mode: AuthMode::ApiKeys,
            secret: "cluster-secret".into(),
            keys: vec![ApiKey {
                key: "sk-alpha".into(),
                tenant: "team-a".into(),
                enabled: true,
                mode: None,
            }],
        })
    }

    #[test]
    fn every_documented_credential_header_resolves_the_same_tenant() {
        for header in ["authorization", "x-api-key", "x-kompressor-key"] {
            let value = if header == "authorization" {
                "Bearer sk-alpha".to_string()
            } else {
                "sk-alpha".to_string()
            };
            let identity = resolve(&keyed(), &headers(&[(header, &value)]))
                .unwrap_or_else(|_| panic!("{header} must be read"));
            assert_eq!(identity.tenant.as_str(), "team-a");
        }
    }

    #[test]
    fn a_bearer_scheme_is_matched_without_regard_to_case() {
        assert_eq!(strip_bearer("bearer sk-alpha"), Some("sk-alpha"));
        assert_eq!(strip_bearer("BEARER  sk-alpha"), Some("sk-alpha"));
        assert_eq!(strip_bearer("sk-alpha"), Some("sk-alpha"));
        assert_eq!(strip_bearer("Basic sk-alpha"), None);
        assert_eq!(strip_bearer("Bearer "), None);
    }

    #[test]
    fn a_tenant_header_is_honoured_only_alongside_the_shared_secret() {
        let borrowed = resolve(
            &keyed(),
            &headers(&[("x-api-key", "sk-alpha"), ("x-kompressor-tenant", "team-b")]),
        )
        .unwrap();
        assert_eq!(
            borrowed.tenant.as_str(),
            "team-a",
            "a key holder cannot reach another tenant by asking"
        );

        let trusted = resolve(
            &keyed(),
            &headers(&[
                ("x-kompressor-secret", "cluster-secret"),
                ("x-kompressor-tenant", "team-b"),
            ]),
        )
        .unwrap();
        assert_eq!(trusted.tenant.as_str(), "team-b");
    }

    #[test]
    fn a_request_with_no_credential_is_refused_when_keys_are_configured() {
        assert_eq!(
            resolve(&keyed(), &HeaderMap::new()),
            Err(AuthError::Missing)
        );
    }
}
