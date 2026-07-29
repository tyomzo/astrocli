//! Bearer-token auth and the SEC-01 startup refusal — SDD §4.5.
//!
//! Two things live here, and the separation is deliberate:
//!
//! * [`AuthPolicy::resolve`] is the **startup** decision (SEC-01). It is a pure function of the
//!   configured token and the bind address, which is what makes it exhaustively testable — the
//!   refusal is the one security property of this milestone that must never regress quietly.
//! * [`require_bearer`] is the **per-request** decision (SEC-02): every route, including
//!   `/api/ingest` and the WS upgrades of SDD §5.11.1, sits behind it. The frames the field
//!   node uploads arrive through the same door as everything else.
//!
//! # Why a startup refusal at all
//!
//! A node with no token bound to `0.0.0.0` is reachable, unauthenticated, from every interface
//! the VPN attaches. On this node that means an open `/api/ingest`: anyone on the VPN could
//! write into the authoritative session archive (IPP-09, REL-13). Refusing at startup is the
//! earliest point the mistake can be caught. The loopback exception exists so a developer can
//! run the binary with no environment set up at all, and nothing else.
//!
//! # Why the token is hashed before comparison
//!
//! `subtle::ConstantTimeEq` on the raw bytes returns early when the lengths differ, so a
//! byte-for-byte comparison leaks the token length through timing. Comparing SHA-256 digests
//! makes both operands exactly 32 bytes, so neither the length nor the content of the configured
//! token is observable from response timing.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use astroctl_core::error::{ApiError, ErrorCode};
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// SHA-256 of the configured bearer token.
///
/// Never printed: the digest of a short or low-entropy token is a perfectly good oracle for
/// guessing it offline, so it is redacted exactly like the [`astroctl_core::config::Secret`] it
/// came from.
#[derive(Clone)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    /// Hash a token.
    #[must_use]
    pub fn of(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }

    /// Constant-time comparison against a presented token.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        self.0.ct_eq(&presented).into()
    }
}

impl fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenDigest(<redacted>)")
    }
}

/// The node's authentication posture, decided once at startup (SDD §4.5).
#[derive(Clone, Debug)]
pub enum AuthPolicy {
    /// A token is configured; every request must present it (SEC-02).
    Enforced(TokenDigest),
    /// No token is configured and the node is bound to a loopback address, so nothing off this
    /// machine can reach it. The SEC-01 exception, and the only way this variant is reachable —
    /// see [`AuthPolicy::resolve`].
    OpenOnLoopback,
}

/// The node refused to start. Rendered to stderr before the process exits non-zero.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StartupRefusal(String);

impl AuthPolicy {
    /// Decide the posture, or refuse to start (SEC-01).
    ///
    /// `token` is the value of the variable named by `server.auth_token_env`, absent if the
    /// variable is unset. An empty or whitespace-only token counts as absent: a zero-length
    /// secret authenticates nobody, and treating it as "configured" would turn the refusal below
    /// into a formality that an `export ASTROCTL_TOKEN=` satisfies.
    ///
    /// # Errors
    /// [`StartupRefusal`] when no usable token is configured and `bind` is not a loopback
    /// address — the SEC-01 rule this whole module exists for.
    pub fn resolve(
        token_env: &str,
        token: Option<&str>,
        bind: IpAddr,
    ) -> Result<Self, StartupRefusal> {
        match token.map(str::trim).filter(|t| !t.is_empty()) {
            Some(token) => Ok(Self::Enforced(TokenDigest::of(token))),
            None if bind.is_loopback() => Ok(Self::OpenOnLoopback),
            None => Err(StartupRefusal(format!(
                "refusing to start: no authentication token and the API would be reachable from \
                 the network.\n  \
                 `server.host` is {bind}, which is not a loopback address, and the environment \
                 variable `{token_env}` named by `server.auth_token_env` is not set (or is \
                 empty).\n  \
                 Fix it either way:\n    \
                 export {token_env}=\"$(head -c 32 /dev/urandom | base64)\"   # authenticate the \
                 API (SEC-02)\n    \
                 …or set `server.host: 127.0.0.1` in the configuration file if this node is \
                 meant to be unreachable from the network.\n  \
                 An unauthenticated node on a VPN is reachable by every host on it (SEC-01)."
            ))),
        }
    }

    /// Whether requests must present a token. Reported by `/api/system/info` so an operator can
    /// see the posture without reading the environment of a running process.
    #[must_use]
    pub const fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforced(_))
    }

    /// Check one request's credentials.
    ///
    /// # Errors
    /// [`ApiError`] with [`ErrorCode::Auth`] (401) when the header is absent, is not a `Bearer`
    /// credential, or carries the wrong token.
    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let Self::Enforced(expected) = self else {
            return Ok(());
        };

        let presented = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_credential)
            .ok_or_else(|| {
                ApiError::new(
                    ErrorCode::Auth,
                    "missing bearer token: send `Authorization: Bearer <token>`",
                )
            })?;

        if expected.matches(presented) {
            Ok(())
        } else {
            // Deliberately says nothing about the presented value — not its length, not a
            // prefix. The operator's own token is in their environment; an attacker's is not
            // worth confirming a single byte of.
            Err(ApiError::new(ErrorCode::Auth, "invalid bearer token"))
        }
    }
}

/// Extract the credential from an `Authorization` header value.
///
/// The scheme is matched case-insensitively (RFC 9110 §11.1: `Bearer`, `bearer` and `BEARER` are
/// the same scheme) and exactly one space is expected after it, per RFC 6750 §2.1.
fn bearer_credential(header: &str) -> Option<&str> {
    let (scheme, credential) = header.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| credential.trim())
        .filter(|c| !c.is_empty())
}

/// The middleware itself — one layer over every route (SDD §4.5).
///
/// Applied with [`axum::middleware::from_fn_with_state`] on the whole router, so it runs *before*
/// routing: an unauthenticated request cannot even learn which paths exist.
pub async fn require_bearer(
    State(policy): State<Arc<AuthPolicy>>,
    request: Request,
    next: Next,
) -> Response {
    match policy.authorize(request.headers()) {
        Ok(()) => next.run(request).await,
        Err(err) => unauthorized(&err),
    }
}

/// Render a 401 with the SDD §4.2 envelope and the `WWW-Authenticate` challenge RFC 6750 §3
/// requires.
fn unauthorized(err: &ApiError) -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, Json(err)).into_response();
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"astroctl\""),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: &str = "ASTROCTL_TOKEN";

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address parses")
    }

    fn header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(value).expect("test header is valid"),
        );
        headers
    }

    // --- SEC-01: the startup refusal --------------------------------------------------

    /// The whole truth table, because this is the property M0 exists to establish.
    #[test]
    fn startup_refusal_truth_table() {
        let cases: &[(Option<&str>, &str, bool)] = &[
            // token, bind address, expected to start
            (Some("s3cret"), "0.0.0.0", true),
            (Some("s3cret"), "10.8.0.2", true),
            (Some("s3cret"), "127.0.0.1", true),
            (Some("s3cret"), "::1", true),
            // No token: only loopback is allowed to run.
            (None, "127.0.0.1", true),
            (None, "127.0.0.53", true),
            (None, "::1", true),
            // …and everything else refuses, `0.0.0.0` included: binding every interface is the
            // exact case SEC-01 is about, and it is not "loopback" just because loopback is one
            // of the interfaces it covers.
            (None, "0.0.0.0", false),
            (None, "::", false),
            (None, "10.8.0.2", false),
            (None, "192.168.1.50", false),
            // An empty or whitespace token is not a token.
            (Some(""), "0.0.0.0", false),
            (Some("   "), "0.0.0.0", false),
        ];

        for &(token, bind, should_start) in cases {
            let outcome = AuthPolicy::resolve(ENV, token, ip(bind));
            assert_eq!(
                outcome.is_ok(),
                should_start,
                "token {token:?} bound to {bind} should {} start",
                if should_start { "" } else { "not" }
            );
        }
    }

    #[test]
    fn refusal_message_names_the_variable_the_address_and_both_remedies() {
        let err = AuthPolicy::resolve(ENV, None, ip("10.8.0.2")).expect_err("must refuse");
        let message = err.to_string();
        assert!(message.contains(ENV), "names the env var: {message}");
        assert!(message.contains("10.8.0.2"), "names the address: {message}");
        assert!(message.contains("server.host"), "names the key: {message}");
        assert!(
            message.contains("SEC-01"),
            "cites the requirement: {message}"
        );
    }

    #[test]
    fn a_loopback_node_without_a_token_is_open_and_says_so() {
        let policy = AuthPolicy::resolve(ENV, None, ip("127.0.0.1")).expect("loopback may start");
        assert!(!policy.is_enforced());
        assert!(policy.authorize(&HeaderMap::new()).is_ok());
    }

    // --- SEC-02: the per-request check ------------------------------------------------

    #[test]
    fn enforced_policy_accepts_only_the_configured_token() {
        let policy = AuthPolicy::resolve(ENV, Some("s3cret"), ip("0.0.0.0")).expect("starts");
        assert!(policy.is_enforced());

        assert!(policy.authorize(&header("Bearer s3cret")).is_ok());
        // RFC 9110 §11.1: the scheme is case-insensitive.
        assert!(policy.authorize(&header("bearer s3cret")).is_ok());
        assert!(policy.authorize(&header("BEARER s3cret")).is_ok());

        for rejected in [
            "Bearer wrong",
            "Bearer s3cre",
            "Bearer s3crett",
            "Bearer ",
            "s3cret",
            "Basic czNjcmV0",
            "Bearer s3cret extra",
        ] {
            let err = policy
                .authorize(&header(rejected))
                .expect_err("`{rejected}` must be rejected");
            assert_eq!(err.code, ErrorCode::Auth, "for header `{rejected}`");
            assert_eq!(err.http_status(), 401);
        }

        let err = policy.authorize(&HeaderMap::new()).expect_err("no header");
        assert_eq!(err.code, ErrorCode::Auth);
    }

    #[test]
    fn rejections_never_echo_the_presented_credential() {
        let policy = AuthPolicy::resolve(ENV, Some("s3cret"), ip("0.0.0.0")).expect("starts");
        let err = policy
            .authorize(&header("Bearer hunter2"))
            .expect_err("wrong token");
        assert!(!err.message.contains("hunter2"), "{}", err.message);
        assert!(!err.message.contains("s3cret"), "{}", err.message);
    }

    #[test]
    fn the_digest_is_never_printed() {
        let digest = TokenDigest::of("s3cret");
        assert_eq!(format!("{digest:?}"), "TokenDigest(<redacted>)");
        let policy = AuthPolicy::Enforced(digest);
        assert!(!format!("{policy:?}").contains("s3cret"));
    }

    #[test]
    fn digest_comparison_is_exact() {
        let digest = TokenDigest::of("s3cret");
        assert!(digest.matches("s3cret"));
        assert!(!digest.matches("s3cre"));
        assert!(!digest.matches("S3cret"));
        assert!(!digest.matches(""));
    }

    #[test]
    fn bearer_credential_parsing() {
        assert_eq!(bearer_credential("Bearer abc"), Some("abc"));
        assert_eq!(bearer_credential("Bearer  abc "), Some("abc"));
        assert_eq!(bearer_credential("Bearer"), None);
        assert_eq!(bearer_credential("Bearer "), None);
        assert_eq!(bearer_credential("Token abc"), None);
    }
}
