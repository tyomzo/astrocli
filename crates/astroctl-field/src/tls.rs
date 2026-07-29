//! TLS termination in this process (SEC-05, ADD §4), and the certificate expiry SEC-07 reports.
//!
//! # Why the binary and not a proxy
//!
//! ADD §4: the field node is the only process that must be up for the system to work. A Pi
//! already running the mount and the camera does not need a second process to supervise, and a
//! proxy that fails to start is a rig with no UI at all. The certificate therefore lives here.
//!
//! # Why at all
//!
//! Not confidentiality — the VPN already provides that. Chrome gates `navigator.wakeLock`,
//! service-worker registration and `beforeinstallprompt` behind a *secure context*, so USB-09 and
//! USB-10 are unreachable over plain HTTP at any address other than `localhost`. A VPN does not
//! substitute: the browser judges the origin, and a tunnelled `http://` origin is still insecure.
//! Every one of those APIs works on `http://localhost`, which is what makes this a task rather
//! than a footnote — the whole PWA can be built and demoed without ever meeting the problem.
//!
//! # Shape
//!
//! [`load`] turns a [`TlsConfig`] into a [`Materials`], failing loudly and naming the path.
//! [`Materials::into_listener`] wraps the already-bound `TcpListener` in something that still
//! satisfies [`axum::serve::Listener`], so `main` keeps **one** server call and one graceful
//! shutdown path — the HTTP and HTTPS modes differ by the listener and by nothing else.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use astroctl_core::config::TlsConfig;
use chrono::{DateTime, SecondsFormat, Utc};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Seconds in a day, for the whole-day arithmetic SEC-07 reports in.
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// How many TLS handshakes may be in flight before the node stops accepting new connections.
///
/// The handshake runs off the accept path (see [`TlsListener::accept`]), which is what stops one
/// slow client from stalling every other, but "off the accept path" without a bound is just an
/// unbounded queue of tasks a client can fill by connecting and never speaking. 64 is far above
/// anything a single operator's browser opens and far below anything that costs the Pi memory.
const MAX_PENDING_HANDSHAKES: usize = 64;

/// How long to wait before retrying `accept` after an error that is about this process rather
/// than about one peer — running out of file descriptors, most likely.
///
/// Retrying instantly would spin a core for as long as the condition lasts, and the condition is
/// exactly the sort that lasts.
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// Everything that can stop the node between "the operator configured TLS" and "the node can
/// serve it". Every variant names a path, because the operator's next action is to look at a
/// file and the message has to say which one.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// The file could not be opened or read.
    #[error("cannot read {what} file `{path}`: {source}")]
    Read {
        /// What the file was supposed to hold, for a message that reads as a sentence.
        what: &'static str,
        /// Path from `server.tls`.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },

    /// The file was read but holds no PEM block of the expected kind.
    #[error(
        "`{path}` holds no {what}; expected a PEM file with a `-----BEGIN` block \
         (`server.tls` is configured, so this is a startup failure rather than a fallback to \
         plain HTTP — SEC-05)"
    )]
    Empty {
        /// What was expected.
        what: &'static str,
        /// Path from `server.tls`.
        path: PathBuf,
    },

    /// A PEM block was found but its contents are not decodable.
    #[error("`{path}` is not a valid PEM {what}: {source}")]
    Pem {
        /// What was being parsed.
        what: &'static str,
        /// Path from `server.tls`.
        path: PathBuf,
        /// Decoder failure.
        #[source]
        source: io::Error,
    },

    /// The leaf certificate is not parseable as X.509, so its expiry cannot be reported.
    #[error("`{path}` does not hold a readable X.509 certificate: {reason}")]
    Certificate {
        /// `server.tls.cert_path`.
        path: PathBuf,
        /// What the X.509 parser said.
        reason: String,
    },

    /// The pair loaded but rustls will not serve it — most often a key that does not match the
    /// certificate, or an algorithm the compiled-in provider does not implement.
    #[error(
        "the certificate in `{cert_path}` and the key in `{key_path}` cannot be served \
         together: {source}"
    )]
    Rejected {
        /// `server.tls.cert_path`.
        cert_path: PathBuf,
        /// `server.tls.key_path`.
        key_path: PathBuf,
        /// What rustls said.
        #[source]
        source: tokio_rustls::rustls::Error,
    },
}

/// The served certificate's expiry, and the threshold at which SEC-07 calls it a problem.
///
/// Holds the deadline rather than a remaining count: "days remaining" is a function of the clock
/// and would be wrong by one every midnight if it were computed once at startup. `now` is a
/// parameter for the same reason it is a parameter in `vitals` — a test that has to wait for a
/// certificate to age is a test nobody runs.
#[derive(Clone, Copy, Debug)]
pub struct CertificateStatus {
    expires_at: DateTime<Utc>,
    warn_days: i64,
}

impl CertificateStatus {
    /// `notAfter` of the leaf certificate.
    ///
    /// Test-only. Production reports the rendered form and the derived day count, so an accessor
    /// for the raw value would be dead code — and dead code is denied workspace-wide. The tests
    /// need it to express their expectations relative to whatever the fixture's own expiry is,
    /// rather than against a date that would rot.
    #[cfg(test)]
    pub const fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    /// `notAfter` as SDD §2 writes timestamps: UTC RFC 3339 with milliseconds.
    #[must_use]
    pub fn expires_at_rfc3339(&self) -> String {
        self.expires_at.to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    /// Whole days from `now` until expiry, **negative once it has passed**.
    ///
    /// Floored rather than truncated, so the number only ever understates the time left: 13 hours
    /// before expiry reports `0`, not `1`. An operator who reads "1 day" and comes back tomorrow
    /// is exactly the person SEC-07 exists to protect.
    #[must_use]
    pub fn days_remaining(&self, now: DateTime<Utc>) -> i64 {
        (self.expires_at - now)
            .num_seconds()
            .div_euclid(SECONDS_PER_DAY)
    }

    /// Whether the certificate is inside `warn_days_before_expiry` — or already past `notAfter`.
    ///
    /// An expired certificate revokes the secure context exactly as a missing one does, so it
    /// disables the wake lock and the installed app. It is not a separate, worse state that needs
    /// its own wire value; it is the same warning, still on.
    #[must_use]
    pub fn is_warning(&self, now: DateTime<Utc>) -> bool {
        self.days_remaining(now) < self.warn_days
    }
}

/// A loaded, validated certificate: what the listener needs to accept connections, and what
/// `/api/system/health` needs to report about it.
pub struct Materials {
    acceptor: TlsAcceptor,
    status: CertificateStatus,
}

/// Redacted, and hand-written rather than derived for that reason: the acceptor owns the private
/// key, and the way a key reaches a log line is a `Debug` nobody thought about (SEC-04). The
/// expiry is not secret and is the only part anyone debugging this wants to see.
impl fmt::Debug for Materials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Materials")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Materials {
    /// What SEC-07 reports. `Copy`, so the API layer holds a value rather than a lock.
    #[must_use]
    pub const fn status(&self) -> CertificateStatus {
        self.status
    }

    /// Wrap an already-bound listener so `axum::serve` terminates TLS on it.
    ///
    /// The listener is bound by the caller and not here, so that a port already in use is still
    /// reported by the same code path in both modes.
    #[must_use]
    pub fn into_listener(self, tcp: TcpListener) -> TlsListener {
        TlsListener {
            tcp,
            acceptor: self.acceptor,
            handshakes: JoinSet::new(),
        }
    }
}

/// Load the certificate and key named by `server.tls`.
///
/// # Errors
///
/// [`TlsError`] if either file is unreadable, holds no PEM material, holds material that does not
/// decode, or holds a pair rustls will not serve. Every case is a startup failure by design: an
/// operator who asked for TLS and silently got plaintext has a PWA that will not install and no
/// symptom pointing at the cause.
pub fn load(config: &TlsConfig) -> Result<Materials, TlsError> {
    let chain = read_certificates(&config.cert_path)?;
    let key = read_private_key(&config.key_path)?;

    // The leaf is first by PEM convention and by every ACME client's output. Reading its expiry
    // before the handshake machinery is built means a certificate whose dates cannot be parsed is
    // caught at startup rather than discovered when SEC-07 has nothing to report.
    let expires_at = leaf_expiry(&chain[0], &config.cert_path)?;

    install_crypto_provider();

    let mut server = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|source| TlsError::Rejected {
            cert_path: config.cert_path.clone(),
            key_path: config.key_path.clone(),
            source,
        })?;

    // Advertise HTTP/1.1 only. Not a performance judgement — the PWA shell is one bundle, so
    // there is little for HTTP/2 multiplexing to win — but a scope one: M1 puts a WebSocket hub
    // and a live-view stream on this link, and WebSockets over HTTP/2 need RFC 8441 extended
    // CONNECT, which this stack does not implement. Naming one protocol makes the negotiation
    // deterministic instead of leaving it to whatever the client offers.
    server.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Materials {
        acceptor: TlsAcceptor::from(Arc::new(server)),
        status: CertificateStatus {
            expires_at,
            warn_days: i64::from(config.warn_days_before_expiry),
        },
    })
}

/// Install the `ring` crypto provider as this process's default.
///
/// rustls 0.23 resolves its provider lazily from crate features and **panics at first use** if
/// the choice is ambiguous — which surfaces as the first handshake killing a worker thread rather
/// than as a build error. Installing it explicitly, before any `ServerConfig` is built, makes the
/// choice this crate's rather than the dependency graph's.
///
/// A second call is a no-op: `install_default` reports that one is already installed, and the
/// only way that happens is that this function ran twice, so there is nothing to warn about.
fn install_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

fn open(what: &'static str, path: &Path) -> Result<io::BufReader<std::fs::File>, TlsError> {
    std::fs::File::open(path)
        .map(io::BufReader::new)
        .map_err(|source| TlsError::Read {
            what,
            path: path.to_path_buf(),
            source,
        })
}

/// The PEM chain, leaf first.
///
/// The chain and not just the leaf: a leaf on its own validates on a desktop that has already
/// cached the intermediate and fails on the phone that has not, which is the harder of the two
/// failures to reproduce and the only one that matters here.
fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let mut reader = open("certificate", path)?;
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsError::Pem {
            what: "certificate",
            path: path.to_path_buf(),
            source,
        })?;

    // A file with no `-----BEGIN CERTIFICATE-----` at all yields an empty iterator rather than an
    // error, so "the operator pointed this at a README" arrives here and nowhere else.
    if chain.is_empty() {
        return Err(TlsError::Empty {
            what: "certificate",
            path: path.to_path_buf(),
        });
    }
    Ok(chain)
}

/// The private key, in whichever of the three PEM encodings the issuer produced.
///
/// `rustls_pemfile::private_key` accepts PKCS#8, PKCS#1 **and SEC1**. The last is not a
/// completeness note: Let's Encrypt via acme.sh issues ECDSA by default, and an ECDSA key is
/// `-----BEGIN EC PRIVATE KEY-----`. A loader written against a generated RSA test key would
/// pass its own tests and fail on the only certificate this deployment has.
fn read_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut reader = open("private key", path)?;
    rustls_pemfile::private_key(&mut reader)
        .map_err(|source| TlsError::Pem {
            what: "private key",
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| TlsError::Empty {
            what: "private key",
            path: path.to_path_buf(),
        })
}

/// `notAfter` of the leaf, for SEC-07.
///
/// rustls parses a certificate only as far as a handshake requires and exposes no validity
/// accessor, so reporting expiry needs a real X.509 parser rather than a cast.
fn leaf_expiry(leaf: &CertificateDer<'_>, path: &Path) -> Result<DateTime<Utc>, TlsError> {
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref()).map_err(|error| {
        TlsError::Certificate {
            path: path.to_path_buf(),
            reason: error.to_string(),
        }
    })?;

    let seconds = parsed.validity().not_after.timestamp();
    DateTime::from_timestamp(seconds, 0).ok_or_else(|| TlsError::Certificate {
        path: path.to_path_buf(),
        reason: format!("`notAfter` is {seconds}s from the epoch, which is not a real date"),
    })
}

// ---------------------------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------------------------

/// A `TcpListener` that hands `axum::serve` connections which have already completed their TLS
/// handshake.
///
/// Implementing [`axum::serve::Listener`] rather than reaching for a second server crate is what
/// keeps `main` at one `axum::serve` call: the graceful-shutdown ordering of SDD §7 is written
/// once and applies to both modes, instead of existing twice and drifting.
pub struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    /// Handshakes in flight. They are *not* awaited inside `accept`, because a handshake is the
    /// one part of accepting a connection that the peer controls the timing of.
    handshakes: JoinSet<Handshake>,
}

/// What a handshake task hands back: a connection ready to carry HTTP, or `None` because the
/// handshake failed — which the task logs itself, where the peer's address is still in scope.
type Handshake = Option<(tokio_rustls::server::TlsStream<TcpStream>, SocketAddr)>;

/// Unwrap a joined handshake task, reporting the one outcome that is this node's fault.
fn harvest(joined: Option<Result<Handshake, tokio::task::JoinError>>) -> Handshake {
    match joined {
        Some(Ok(ready)) => ready,
        Some(Err(error)) => {
            tracing::warn!(%error, "a TLS handshake task did not finish");
            None
        }
        // Unreachable: `accept` never polls the set while it is empty.
        None => None,
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    /// Return the next connection that is ready to carry HTTP.
    ///
    /// The handshake happens in a spawned task rather than inline. Inline is the obvious
    /// implementation and it is wrong: a peer that opens a socket and sends nothing would hold
    /// the accept loop for as long as it liked, and one such peer is enough to make the node
    /// unreachable. Nothing here can return an error, because the trait's contract is that a
    /// listener retries rather than reporting — an accept loop that gives up is a node that
    /// needs a restart to answer the operator again.
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // Destructured so the two waits below borrow different fields; `select!` over
        // `&mut self` twice would not compile, and threading them through a helper would hide
        // which of them is cancelled.
        let Self {
            tcp,
            acceptor,
            handshakes,
        } = self;

        loop {
            let accepted = if handshakes.is_empty() {
                // `join_next` on an empty set is ready *immediately* with `None`. Inside the
                // `select!` below that would spin, so an empty set is its own case rather than a
                // guard on the macro arm.
                Some(accept_tcp(tcp).await)
            } else if handshakes.len() >= MAX_PENDING_HANDSHAKES {
                // Back-pressure: stop taking connections until one clears. The kernel's listen
                // backlog is a better place to queue than a `JoinSet` a peer can grow for free.
                if let Some(ready) = harvest(handshakes.join_next().await) {
                    return ready;
                }
                None
            } else {
                // Both arms are cancel-safe, which is what makes the loser of the race free to
                // drop: a cancelled `TcpListener::accept` leaves the connection in the backlog,
                // and a cancelled `JoinSet::join_next` does not lose a finished task.
                tokio::select! {
                    joined = handshakes.join_next() => {
                        if let Some(ready) = harvest(joined) {
                            return ready;
                        }
                        None
                    }
                    pair = accept_tcp(tcp) => Some(pair),
                }
            };

            // Spawned out here rather than in the `select!` arm: `join_next` holds a mutable
            // borrow of the set for the whole macro, so the spawn cannot live inside it.
            if let Some((stream, peer)) = accepted {
                let acceptor = acceptor.clone();
                handshakes.spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls) => Some((tls, peer)),
                        Err(error) => {
                            // Ordinary traffic, not an incident: an `http://` request sent to
                            // this port lands here, and so does every scanner on the VPN.
                            // Recorded at debug so it is there when someone asks why their
                            // browser says "connection reset", and invisible otherwise.
                            tracing::debug!(%peer, %error, "TLS handshake failed");
                            None
                        }
                    }
                });
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Accept one TCP connection, retrying past errors the way `axum`'s own listener does.
async fn accept_tcp(tcp: &TcpListener) -> (TcpStream, SocketAddr) {
    loop {
        match tcp.accept().await {
            Ok(pair) => return pair,
            Err(error) if is_peer_error(&error) => {
                // The peer went away between the SYN and the accept. Nothing happened to this
                // node; the next connection is unaffected.
            }
            Err(error) => {
                tracing::warn!(%error, backoff_s = ACCEPT_BACKOFF.as_secs(), "cannot accept a connection");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

/// Whether an `accept` error is about the connection that failed rather than about this process.
fn is_peer_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    /// Fixtures live beside the crate rather than being generated, so the material under test is
    /// byte-stable across runs and machines — a key generated in the test would be a different
    /// key every time, and the encoding is precisely what is being asserted.
    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name)
    }

    fn config(cert: &str, key: &str, warn_days: u32) -> TlsConfig {
        TlsConfig {
            cert_path: testdata(cert),
            key_path: testdata(key),
            warn_days_before_expiry: warn_days,
        }
    }

    // --- key encodings ------------------------------------------------------------------

    /// The trap this task was warned about: acme.sh issues ECDSA by default and an ECDSA key is
    /// SEC1, not PKCS#8. A loader tested only against a generated RSA key passes and then fails
    /// on the only certificate the deployment has.
    #[test]
    fn an_ecdsa_certificate_with_a_sec1_key_loads() {
        let materials = load(&config("ecdsa-sec1.crt.pem", "ecdsa-sec1.key.pem", 14))
            .expect("an ECDSA/SEC1 pair must load");
        assert!(
            materials.status().expires_at().timestamp() > 0,
            "the leaf's notAfter must have been read, not defaulted"
        );
    }

    #[test]
    fn an_rsa_certificate_with_a_pkcs8_key_loads() {
        load(&config("rsa-pkcs8.crt.pem", "rsa-pkcs8.key.pem", 14))
            .expect("an RSA/PKCS#8 pair must load");
    }

    // --- failing loudly -----------------------------------------------------------------

    #[test]
    fn a_missing_certificate_names_the_path() {
        let cfg = config("does-not-exist.pem", "ecdsa-sec1.key.pem", 14);
        let error = load(&cfg).expect_err("a missing certificate must not start the node");
        assert!(
            error.to_string().contains("does-not-exist.pem"),
            "the operator's next action is to look at a file, so the message must name it: {error}"
        );
    }

    #[test]
    fn a_missing_key_names_the_path() {
        let cfg = config("ecdsa-sec1.crt.pem", "does-not-exist.key.pem", 14);
        let error = load(&cfg).expect_err("a missing key must not start the node");
        assert!(
            error.to_string().contains("does-not-exist.key.pem"),
            "{error}"
        );
    }

    /// The likelier operator error than corruption: the path points at a real, readable file that
    /// simply is not a certificate.
    #[test]
    fn a_file_that_is_not_a_certificate_names_the_path() {
        let cfg = config("not-a-certificate.pem", "ecdsa-sec1.key.pem", 14);
        let error = load(&cfg).expect_err("a non-certificate must not start the node");
        assert!(
            error.to_string().contains("not-a-certificate.pem"),
            "{error}"
        );
        assert!(
            error.to_string().contains("no certificate"),
            "the message has to distinguish `wrong file` from `unreadable file`: {error}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_key_names_the_path() {
        let cfg = config("ecdsa-sec1.crt.pem", "not-a-certificate.pem", 14);
        let error = load(&cfg).expect_err("a non-key must not start the node");
        assert!(error.to_string().contains("no private key"), "{error}");
    }

    /// A certificate and a key that are each individually valid but belong to different pairs.
    /// rustls is what catches this, and the message has to survive being wrapped.
    #[test]
    fn a_mismatched_certificate_and_key_are_refused() {
        let cfg = config("ecdsa-sec1.crt.pem", "rsa-pkcs8.key.pem", 14);
        let error = load(&cfg).expect_err("a mismatched pair must not start the node");
        let message = error.to_string();
        assert!(message.contains("ecdsa-sec1.crt.pem"), "{message}");
        assert!(message.contains("rsa-pkcs8.key.pem"), "{message}");
    }

    // --- SEC-07 -------------------------------------------------------------------------

    /// Expressed relative to the fixture's own `notAfter` rather than against a hardcoded date,
    /// so regenerating the fixture never turns this into a failing test about nothing.
    #[test]
    fn days_remaining_counts_down_and_goes_negative() {
        let status = load(&config("ecdsa-sec1.crt.pem", "ecdsa-sec1.key.pem", 14))
            .expect("loads")
            .status();
        let expiry = status.expires_at();

        assert_eq!(status.days_remaining(expiry - TimeDelta::days(30)), 30);
        assert_eq!(status.days_remaining(expiry - TimeDelta::days(1)), 1);
        // Floored, not rounded: 13 hours left is zero whole days, never one.
        assert_eq!(status.days_remaining(expiry - TimeDelta::hours(13)), 0);
        assert_eq!(status.days_remaining(expiry + TimeDelta::hours(1)), -1);
    }

    #[test]
    fn the_warning_opens_at_the_configured_threshold_and_stays_open_past_expiry() {
        let status = load(&config("ecdsa-sec1.crt.pem", "ecdsa-sec1.key.pem", 14))
            .expect("loads")
            .status();
        let expiry = status.expires_at();

        assert!(!status.is_warning(expiry - TimeDelta::days(15)));
        // 14 days remaining is not yet inside a 14-day threshold; 13 is.
        assert!(!status.is_warning(expiry - TimeDelta::days(14)));
        assert!(status.is_warning(expiry - TimeDelta::days(13)));
        assert!(status.is_warning(expiry - TimeDelta::hours(1)));
        // An expired certificate revokes the secure context exactly as a missing one does, so it
        // is the same warning rather than a state that quietly resets.
        assert!(status.is_warning(expiry + TimeDelta::days(1)));
    }

    #[test]
    fn the_threshold_is_the_operators_and_not_a_constant() {
        let generous = load(&config("ecdsa-sec1.crt.pem", "ecdsa-sec1.key.pem", 60))
            .expect("loads")
            .status();
        let expiry = generous.expires_at();
        assert!(generous.is_warning(expiry - TimeDelta::days(30)));

        let terse = load(&config("ecdsa-sec1.crt.pem", "ecdsa-sec1.key.pem", 2))
            .expect("loads")
            .status();
        assert!(!terse.is_warning(expiry - TimeDelta::days(30)));
    }

    #[test]
    fn the_expiry_is_rendered_the_way_sdd_2_writes_timestamps() {
        let status = load(&config("ecdsa-sec1.crt.pem", "ecdsa-sec1.key.pem", 14))
            .expect("loads")
            .status();
        let rendered = status.expires_at_rfc3339();
        assert!(
            rendered.ends_with('Z'),
            "UTC, not a local offset: {rendered}"
        );
        assert!(
            rendered.contains(".000"),
            "milliseconds, per SDD §2: {rendered}"
        );
        assert_eq!(
            rendered.parse::<DateTime<Utc>>().expect("round-trips"),
            status.expires_at()
        );
    }
}
