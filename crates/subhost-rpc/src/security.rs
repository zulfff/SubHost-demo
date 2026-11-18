//! Bearer-token authentication, bind policy and a per-connection rate limit.
//!
//! The JSON-RPC server has no notion of a caller. Everything it can see is the
//! HTTP request, so the policy is enforced there, before any method runs:
//!
//! - a configured token must be presented as `Authorization: Bearer <token>`;
//!   the comparison is constant time over BLAKE3 digests, so the check does not
//!   leak where the first differing byte is;
//! - a configured rate limit is a token bucket per **connection**. jsonrpsee
//!   0.24 does not expose the peer address to HTTP middleware (it drops the
//!   accepted `remote_addr` before the request is built), so a per-IP bucket
//!   would only be guesswork. One connection is one bucket; `max_connections`
//!   bounds how many buckets a caller can create;
//! - a non-loopback bind needs both [`RpcSecurityConfig::allow_public`] and a
//!   token, so "exposed by accident" and "exposed without auth" are refused
//!   before the server is built rather than warned about.
//!
//! WebSocket connections are authorized once, at the upgrade request. Messages
//! sent afterwards do not pass through this middleware, so the rate limit does
//! not apply per message on a WebSocket session.

use http::{header, HeaderValue, StatusCode};
use jsonrpsee::server::{HttpBody, HttpRequest, HttpResponse};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tower::{Layer, Service};

/// Shortest accepted bearer token. A 16-byte token still has 128 bits of entropy
/// when it was generated randomly; shorter values are usually placeholders.
pub const MIN_TOKEN_BYTES: usize = 16;

/// JSON-RPC error code for a rate-limited request, matching the common
/// "limit exceeded" code used by Ethereum clients.
const RATE_LIMITED_CODE: i32 = -32005;
/// JSON-RPC error code for a rejected bearer token.
const UNAUTHORIZED_CODE: i32 = -32000;

/// Something wrong with the configured token or rate limit.
#[derive(Debug, Error)]
pub enum SecurityError {
    #[error(transparent)]
    Token(#[from] TokenError),

    #[error("the RPC rate limit must be at least 1 request per second")]
    ZeroRateLimit,
}

/// A bearer token could not be loaded or is too weak to use.
#[derive(Debug, Error)]
pub enum TokenError {
    #[error("the RPC auth token is empty")]
    Empty,

    #[error("the RPC auth token is {actual} bytes; at least {minimum} are required")]
    TooShort { actual: usize, minimum: usize },

    #[error("cannot read the RPC auth token from {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("the RPC auth token is not valid UTF-8 text")]
    NotText,
}

/// A bearer token, kept as its plaintext for clients and as a BLAKE3 digest for
/// the constant-time server check.
///
/// [`Debug`] never prints either form, and the plaintext is zeroed on drop.
#[derive(Clone)]
pub struct SecretToken {
    digest: [u8; 32],
    token: String,
}

impl SecretToken {
    /// Hash a token for later comparison.
    pub fn new(token: impl AsRef<[u8]>) -> Result<Self, TokenError> {
        let token = token.as_ref();
        if token.is_empty() {
            return Err(TokenError::Empty);
        }
        if token.len() < MIN_TOKEN_BYTES {
            return Err(TokenError::TooShort { actual: token.len(), minimum: MIN_TOKEN_BYTES });
        }
        Ok(Self {
            digest: *blake3::hash(token).as_bytes(),
            token: String::from_utf8(token.to_vec()).map_err(|_| TokenError::NotText)?,
        })
    }

    /// Read a token from `path`, trimming the trailing newline an editor adds.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, TokenError> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path)
            .map_err(|source| TokenError::Read { path: path.to_path_buf(), source })?;
        Self::new(contents.trim())
    }

    /// Constant-time check of a presented token.
    #[must_use]
    pub fn verify(&self, presented: &[u8]) -> bool {
        let digest = blake3::hash(presented);
        self.digest.ct_eq(digest.as_bytes()).into()
    }

    /// The plaintext, for an RPC client that has to send it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.token
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.token.zeroize();
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(<redacted>)")
    }
}

/// The transport policy for the JSON-RPC server.
///
/// The default is today's behaviour: no token, no rate limit, loopback only.
#[derive(Clone, Debug, Default)]
pub struct RpcSecurityConfig {
    /// Bearer token required on every request when set.
    pub auth_token: Option<SecretToken>,
    /// Requests per second allowed per connection when set.
    pub rate_limit_per_sec: Option<u32>,
    /// Permit a non-loopback bind. Requires [`Self::auth_token`].
    pub allow_public: bool,
}

impl RpcSecurityConfig {
    /// Reject configurations that would silently do nothing.
    pub fn validate(&self) -> Result<(), SecurityError> {
        if self.rate_limit_per_sec == Some(0) {
            return Err(SecurityError::ZeroRateLimit);
        }
        Ok(())
    }
}

/// Why a requested bind is refused.
#[derive(Debug, Error)]
pub enum BindPolicyError {
    #[error(
        "refusing to bind JSON-RPC to the non-loopback address {addr}: \
         pass --rpc-allow-public to expose it deliberately"
    )]
    OptInRequired { addr: SocketAddr },

    #[error(
        "refusing to bind JSON-RPC to the public address {addr} without authentication: \
         configure a bearer token with --rpc-auth-token-file"
    )]
    AuthRequired { addr: SocketAddr },
}

/// Enforce `allow_public` + token before the socket is opened.
pub(crate) fn check_bind_policy(
    addr: SocketAddr,
    security: &RpcSecurityConfig,
) -> Result<(), BindPolicyError> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    if !security.allow_public {
        return Err(BindPolicyError::OptInRequired { addr });
    }
    if security.auth_token.is_none() {
        return Err(BindPolicyError::AuthRequired { addr });
    }
    Ok(())
}

/// The tower layer applied to every HTTP request.
#[derive(Clone)]
pub(crate) struct RpcGuardLayer {
    token: Option<SecretToken>,
    rate_limit: Option<u32>,
}

impl RpcGuardLayer {
    pub(crate) fn new(security: &RpcSecurityConfig) -> Self {
        Self { token: security.auth_token.clone(), rate_limit: security.rate_limit_per_sec }
    }
}

impl<S> Layer<S> for RpcGuardLayer {
    type Service = RpcGuardService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcGuardService {
            inner,
            token: self.token.clone(),
            // Hyper clones the service for every request, so the bucket has to be
            // shared between clones. The layer is applied once per connection,
            // which makes the shared bucket a per-connection bucket.
            bucket: self.rate_limit.map(|rate| Arc::new(Mutex::new(TokenBucket::new(rate)))),
        }
    }
}

/// The per-connection service: authentication, then the rate bucket, then the
/// JSON-RPC service.
#[derive(Clone)]
pub(crate) struct RpcGuardService<S> {
    inner: S,
    token: Option<SecretToken>,
    bucket: Option<Arc<Mutex<TokenBucket>>>,
}

/// Error type jsonrpsee's HTTP middleware expects.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl<S, B> Service<HttpRequest<B>> for RpcGuardService<S>
where
    S: Service<HttpRequest<B>, Response = HttpResponse<HttpBody>>,
    S::Error: Into<BoxError>,
    S::Future: Send + 'static,
{
    type Response = HttpResponse<HttpBody>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: HttpRequest<B>) -> Self::Future {
        if let Some(token) = &self.token {
            let presented = bearer_token(&request);
            let authorized = presented.is_some_and(|presented| token.verify(presented.as_bytes()));
            if !authorized {
                return Box::pin(async {
                    Ok(error_response(
                        StatusCode::UNAUTHORIZED,
                        UNAUTHORIZED_CODE,
                        "unauthorized: a valid bearer token is required",
                        None,
                    ))
                });
            }
        }

        if let Some(bucket) = &self.bucket {
            let mut bucket = bucket.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if !bucket.take() {
                let retry_after = bucket.retry_after_secs();
                return Box::pin(async move {
                    Ok(error_response(
                        StatusCode::TOO_MANY_REQUESTS,
                        RATE_LIMITED_CODE,
                        "rate limit exceeded",
                        Some(retry_after),
                    ))
                });
            }
        }

        let future = self.inner.call(request);
        Box::pin(async move { future.await.map_err(Into::into) })
    }
}

/// Extract the token from `Authorization: Bearer <token>` (scheme is
/// case-insensitive per RFC 7235).
fn bearer_token<B>(request: &HttpRequest<B>) -> Option<&str> {
    let value = request.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

fn error_response(
    status: StatusCode,
    code: i32,
    message: &'static str,
    retry_after: Option<u64>,
) -> HttpResponse<HttpBody> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": serde_json::Value::Null,
        "error": { "code": code, "message": message },
    })
    .to_string();

    let mut builder = HttpResponse::builder()
        .status(status)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if status == StatusCode::UNAUTHORIZED {
        builder = builder.header(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    if let Some(seconds) = retry_after {
        builder = builder.header(header::RETRY_AFTER, seconds.to_string());
    }
    builder.body(HttpBody::from(body)).unwrap_or_else(|_| HttpResponse::new(HttpBody::from("{}")))
}

/// A token bucket that starts full, refills at `rate_per_sec`, and holds at most
/// one second of burst. Cloning copies the current fill level, which only matters
/// if jsonrpsee clones the per-connection service.
#[derive(Clone)]
struct TokenBucket {
    rate_per_sec: f64,
    capacity: f64,
    tokens: f64,
    last: std::time::Instant,
}

impl TokenBucket {
    fn new(rate_per_sec: u32) -> Self {
        let rate = f64::from(rate_per_sec.max(1));
        Self { rate_per_sec: rate, capacity: rate, tokens: rate, last: std::time::Instant::now() }
    }

    fn take(&mut self) -> bool {
        self.take_at(std::time::Instant::now())
    }

    fn take_at(&mut self, now: std::time::Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        self.tokens = (self.tokens + elapsed.as_secs_f64() * self.rate_per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn retry_after_secs(&self) -> u64 {
        let wait = (1.0 - self.tokens).max(0.0) / self.rate_per_sec;
        wait.ceil().max(1.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_token_is_stored_as_a_digest_and_compared_in_constant_time() {
        let token = SecretToken::new("correct horse battery staple").unwrap();
        assert!(token.verify(b"correct horse battery staple"));
        assert!(!token.verify(b"correct horse battery stapl"));
        assert!(!token.verify(b"Correct horse battery staple"));
        assert!(!format!("{token:?}").contains("horse"));
        assert_eq!(token.expose(), "correct horse battery staple");
    }

    #[test]
    fn a_short_or_empty_token_is_refused() {
        assert!(matches!(SecretToken::new(""), Err(TokenError::Empty)));
        assert!(matches!(
            SecretToken::new("too short"),
            Err(TokenError::TooShort { actual: 9, minimum: 16 })
        ));
        assert!(SecretToken::new("0123456789abcdef").is_ok());
    }

    #[test]
    fn a_token_file_is_trimmed_and_a_missing_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "0123456789abcdef\n").unwrap();
        let token = SecretToken::from_file(&path).unwrap();
        assert!(token.verify(b"0123456789abcdef"));

        let missing = SecretToken::from_file(dir.path().join("absent"));
        assert!(matches!(missing, Err(TokenError::Read { .. })));
    }

    #[test]
    fn the_debug_output_never_prints_the_token() {
        let config = RpcSecurityConfig {
            auth_token: Some(SecretToken::new("0123456789abcdef").unwrap()),
            rate_limit_per_sec: Some(10),
            allow_public: true,
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("0123456789abcdef"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn a_zero_rate_limit_is_invalid() {
        let config = RpcSecurityConfig { rate_limit_per_sec: Some(0), ..Default::default() };
        assert!(matches!(config.validate(), Err(SecurityError::ZeroRateLimit)));
    }

    #[test]
    fn a_public_bind_needs_opt_in_and_a_token() {
        let public: SocketAddr = "0.0.0.0:8545".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:8545".parse().unwrap();
        assert!(check_bind_policy(loopback, &RpcSecurityConfig::default()).is_ok());
        assert!(matches!(
            check_bind_policy(public, &RpcSecurityConfig::default()),
            Err(BindPolicyError::OptInRequired { .. })
        ));
        let opt_in = RpcSecurityConfig { allow_public: true, ..Default::default() };
        assert!(matches!(
            check_bind_policy(public, &opt_in),
            Err(BindPolicyError::AuthRequired { .. })
        ));
        let with_token = RpcSecurityConfig {
            allow_public: true,
            auth_token: Some(SecretToken::new("0123456789abcdef").unwrap()),
            ..Default::default()
        };
        assert!(check_bind_policy(public, &with_token).is_ok());
    }

    #[test]
    fn the_bucket_allows_a_burst_then_refills() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(2);
        assert!(bucket.take_at(start));
        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start), "the burst is exhausted");
        assert_eq!(bucket.retry_after_secs(), 1);

        assert!(!bucket.take_at(start + Duration::from_millis(200)));
        assert!(bucket.take_at(start + Duration::from_millis(500)));
        assert!(bucket.take_at(start + Duration::from_secs(1)));
    }

    #[test]
    fn a_slow_rate_reports_the_real_wait() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(1);
        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start));
        assert_eq!(bucket.retry_after_secs(), 1);
        // Half a token is still not enough, but the wait is now under a second,
        // which rounds up to the next whole second in the header.
        let mut bucket = TokenBucket::new(1);
        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start + Duration::from_millis(500)));
        assert_eq!(bucket.retry_after_secs(), 1);
    }
}
