//! Single-use WebSocket tickets — SDD §4.5.
//!
//! # Why this exists at all
//!
//! The browser `WebSocket` constructor takes a URL and a subprotocol list. There is no way to
//! attach an `Authorization` header to the upgrade, so the bearer token that authenticates every
//! other route cannot authenticate `/ws`. §4.5's answer is a short-lived ticket: the client
//! spends its bearer token on `POST /api/auth/ws-ticket`, gets an opaque value, and puts *that*
//! in the query string.
//!
//! The point is not that a ticket is harder to guess than a token — it is that the long-lived
//! token never enters a URL, and therefore never reaches `server.log_dir`, a file the operator
//! may reasonably attach to a support question. A ticket in that file is expired and spent
//! before anyone reads it.
//!
//! # The four rules, and where each is enforced
//!
//! | §4.5 rule | Here |
//! |---|---|
//! | Single use | [`TicketStore::consume`] removes before it validates; a second call misses |
//! | 30 s TTL | [`TICKET_TTL`], checked on consume *and* swept on issue |
//! | ≥128 bits, server-generated | [`Ticket::generate`] — 16 bytes from the OS CSPRNG |
//! | Bounded store | [`MAX_OUTSTANDING`], oldest evicted first |
//!
//! # Why the whole store is a `Mutex<…>` and not something cleverer
//!
//! It holds at most [`MAX_OUTSTANDING`] entries and is touched twice per WebSocket connection.
//! A lock-free structure here would be optimising a path that runs a handful of times a night,
//! and the operations are short enough that no `.await` ever happens inside the guard — which
//! is the property `clippy::await_holding_lock` (denied workspace-wide) actually cares about.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long an issued ticket stays valid (SDD §4.5: "30 s, enough for a slow tunnel and modest
/// clock skew, far too short for a logged value to be worth anything").
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// Hard cap on outstanding tickets (SDD §4.5's "bounded store").
///
/// 64 is far above what any real client needs — the PWA holds at most one at a time, and a
/// second browser tab makes it two — and small enough that the store cannot become a memory
/// growth path for a client that requests tickets and never connects. Reaching the cap evicts
/// the oldest, so an attacker spraying requests denies service to *their own* stale tickets
/// rather than to a legitimate client's fresh one.
const MAX_OUTSTANDING: usize = 64;

/// Bytes of randomness per ticket. SDD §4.5 says "at least 128 bits"; this is exactly that.
const TICKET_BYTES: usize = 16;

/// A ticket value, as it appears in the `?ticket=` query parameter.
///
/// Lowercase hex of [`TICKET_BYTES`] random bytes — 32 characters. Hex rather than base64url
/// because it survives every layer between the browser and here without an encoding question:
/// no padding, no `+`/`/`, nothing a proxy or a log formatter can mangle into a value that
/// almost works.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Ticket(String);

impl Ticket {
    /// Draw a fresh ticket from the OS CSPRNG.
    ///
    /// # Errors
    /// If the OS random source is unavailable. That is not a condition to paper over with a
    /// weaker source: a predictable ticket is an unauthenticated socket, so the caller turns
    /// this into a 500 and the client retries.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; TICKET_BYTES];
        getrandom::getrandom(&mut bytes)?;
        let mut hex = String::with_capacity(TICKET_BYTES * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            // Infallible for a `String`; the `Result` is `fmt::Write`'s, not an I/O one.
            let _ = write!(hex, "{byte:02x}");
        }
        Ok(Self(hex))
    }

    /// The value a client presents, taken as-is.
    #[must_use]
    pub fn from_presented(value: &str) -> Self {
        Self(value.to_owned())
    }

    /// The wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Deliberately opaque: a ticket is a credential, and a credential that prints itself ends up in
/// exactly the log file §4.5 exists to keep it out of.
impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ticket(<redacted>)")
    }
}

/// Why an upgrade was refused.
///
/// Three variants, one message. The distinction is kept in the type because the *hub* wants to
/// log which happened — a burst of `Replayed` is a very different operational story from a burst
/// of `Expired` — while the client is told only "invalid or expired ticket". Telling a caller
/// which of the three their guess was is an oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketRejection {
    /// No `?ticket=` parameter on the upgrade.
    Missing,
    /// Not in the store: never issued, already spent, or swept.
    Unknown,
    /// Present but past its TTL. Consumed anyway — see [`TicketStore::consume`].
    Expired,
}

impl TicketRejection {
    /// The one message every rejection gets.
    pub const MESSAGE: &'static str =
        "a valid, unexpired WebSocket ticket is required; POST /api/auth/ws-ticket for one";

    /// A short tag for the log line, never for the response body.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unknown => "unknown-or-spent",
            Self::Expired => "expired",
        }
    }
}

/// The outstanding-ticket store.
#[derive(Debug)]
pub struct TicketStore {
    inner: Mutex<HashMap<Ticket, Instant>>,
}

impl TicketStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a ticket, sweeping expired entries first.
    ///
    /// The sweep rides on issue rather than on a timer task: tickets only accumulate when
    /// someone asks for them, so the moment a new one arrives is exactly the moment the old ones
    /// are worth collecting, and a node with no clients runs no timer at all.
    ///
    /// # Errors
    /// If the OS random source fails — see [`Ticket::generate`].
    pub fn issue(&self) -> Result<Ticket, getrandom::Error> {
        let ticket = Ticket::generate()?;
        let now = Instant::now();
        {
            let mut store = self.lock();
            store.retain(|_, issued| now.duration_since(*issued) < TICKET_TTL);

            // Still full after the sweep: every outstanding ticket is fresh, so drop the oldest
            // to make room. Refusing the new request instead would let a client that hoards
            // tickets lock out the operator's next reconnect, which is the failure mode that
            // matters on a link that reconnects often (REL-10).
            while store.len() >= MAX_OUTSTANDING {
                let Some(oldest) = store
                    .iter()
                    .min_by_key(|(_, issued)| **issued)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                store.remove(&oldest);
            }
            store.insert(ticket.clone(), now);
        }
        Ok(ticket)
    }

    /// Validate and spend a presented ticket.
    ///
    /// **Removes before it checks the age.** An expired ticket is still deleted, so a client
    /// cannot keep a known-expired value alive as a probe, and there is exactly one code path
    /// by which a ticket leaves the store. `Ok(())` is returned at most once per issued ticket,
    /// which is SDD §4.5's single-use rule stated as a property of this function.
    ///
    /// # Errors
    /// [`TicketRejection`] describing why, for the log; the caller must not pass it to the
    /// client verbatim.
    pub fn consume(&self, presented: Option<&str>) -> Result<(), TicketRejection> {
        let Some(value) = presented else {
            return Err(TicketRejection::Missing);
        };
        let ticket = Ticket::from_presented(value);
        let issued = {
            let mut store = self.lock();
            store.remove(&ticket)
        };
        match issued {
            None => Err(TicketRejection::Unknown),
            Some(issued) if Instant::now().duration_since(issued) >= TICKET_TTL => {
                Err(TicketRejection::Expired)
            }
            Some(_) => Ok(()),
        }
    }

    /// How many tickets are outstanding, expired ones included. Diagnostics and tests only.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Rewind an outstanding ticket's issue time, so a test can reach the TTL boundary without
    /// sleeping for it.
    ///
    /// A test-only door into the one piece of state that is otherwise only reachable by waiting
    /// 30 seconds. The alternative — threading a clock through `issue`/`consume` — would put a
    /// parameter in the production signature that exists solely for tests, and the expiry rule
    /// is worth a real test rather than a 30-second one or none at all.
    #[cfg(test)]
    fn backdate(&self, ticket: &Ticket, by: Duration) {
        let mut store = self.lock();
        if let Some(issued) = store.get_mut(ticket) {
            *issued -= by;
        }
    }

    /// A poisoned lock is not a reason to take the node down.
    ///
    /// Nothing in this module can panic while holding the guard — the operations are a hash
    /// lookup and an insert — so poisoning would have to come from a panic elsewhere in the
    /// process. Recovering the guard keeps the auth path working through it; the alternative is
    /// that one unrelated panic permanently disables every future WebSocket connection.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Ticket, Instant>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issued_ticket_is_accepted_exactly_once() {
        // SDD §4.5's single-use rule, which is the whole reason a ticket is safe in a URL.
        let store = TicketStore::new();
        let ticket = store.issue().expect("issues");

        assert_eq!(store.consume(Some(ticket.as_str())), Ok(()));
        assert_eq!(
            store.consume(Some(ticket.as_str())),
            Err(TicketRejection::Unknown),
            "a replayed ticket must be refused"
        );
    }

    #[test]
    fn a_ticket_carries_at_least_128_bits_and_no_two_are_alike() {
        // The randomness requirement is not checkable by staring at one value, but the two
        // things that would actually go wrong — a short value, and a counter dressed as random
        // — are.
        let store = TicketStore::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let ticket = store.issue().expect("issues");
            assert_eq!(
                ticket.as_str().len(),
                TICKET_BYTES * 2,
                "128 bits of entropy is 32 hex characters"
            );
            assert!(
                ticket.as_str().chars().all(|c| c.is_ascii_hexdigit()),
                "the value goes in a URL query parameter unescaped"
            );
            assert!(seen.insert(ticket.as_str().to_owned()), "tickets repeated");
        }
    }

    #[test]
    fn an_unknown_ticket_is_refused_without_touching_the_store() {
        let store = TicketStore::new();
        let real = store.issue().expect("issues");

        assert_eq!(
            store.consume(Some("00000000000000000000000000000000")),
            Err(TicketRejection::Unknown)
        );
        assert_eq!(store.consume(None), Err(TicketRejection::Missing));
        // The failed guesses must not have spent the legitimate ticket.
        assert_eq!(store.consume(Some(real.as_str())), Ok(()));
    }

    #[test]
    fn a_ticket_older_than_its_ttl_is_refused_and_spent() {
        // SDD §4.5's 30-second rule. The `Expired` distinction matters for the log line; the
        // client is told the same thing either way.
        let store = TicketStore::new();
        let ticket = store.issue().expect("issues");
        store.backdate(&ticket, TICKET_TTL + Duration::from_secs(1));

        assert_eq!(
            store.consume(Some(ticket.as_str())),
            Err(TicketRejection::Expired)
        );
        // Removed even though it was refused: there is one path out of the store, so an expired
        // value cannot be kept around as a probe.
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn a_ticket_exactly_at_the_ttl_is_already_too_old() {
        // The boundary is `>=`, not `>`. Pinned because "expires in 30 s" and "is valid for 30 s"
        // differ by one comparison and nobody notices which one was written.
        let store = TicketStore::new();
        let ticket = store.issue().expect("issues");
        store.backdate(&ticket, TICKET_TTL);
        assert_eq!(
            store.consume(Some(ticket.as_str())),
            Err(TicketRejection::Expired)
        );
    }

    #[test]
    fn issuing_sweeps_tickets_that_expired_while_nobody_was_looking() {
        // The sweep rides on issue, so this is the only thing that keeps a store of abandoned
        // tickets from sitting at the cap all night.
        let store = TicketStore::new();
        let stale = store.issue().expect("issues");
        store.backdate(&stale, TICKET_TTL + Duration::from_secs(1));

        let fresh = store.issue().expect("issues");
        assert_eq!(store.len(), 1, "the stale ticket should have been swept");
        assert_eq!(store.consume(Some(fresh.as_str())), Ok(()));
    }

    #[test]
    fn the_store_stays_bounded_under_a_client_that_never_connects() {
        // SDD §4.5's bounded-store rule. Without the cap this loop is a memory leak reachable by
        // anyone holding the bearer token.
        let store = TicketStore::new();
        for _ in 0..(MAX_OUTSTANDING * 4) {
            store.issue().expect("issues");
        }
        assert!(
            store.len() <= MAX_OUTSTANDING,
            "store grew to {} with a cap of {MAX_OUTSTANDING}",
            store.len()
        );
    }

    #[test]
    fn eviction_takes_the_oldest_so_the_newest_ticket_still_works() {
        // The property that makes the cap safe: a burst of ticket requests must not lock the
        // operator out of the reconnect they are about to make.
        let store = TicketStore::new();
        for _ in 0..MAX_OUTSTANDING {
            store.issue().expect("issues");
        }
        let newest = store.issue().expect("issues");
        assert_eq!(
            store.consume(Some(newest.as_str())),
            Ok(()),
            "the most recently issued ticket must survive eviction"
        );
    }

    #[test]
    fn a_ticket_never_prints_itself() {
        // A credential that appears in a `tracing` field or a panic message defeats the entire
        // reason §4.5 keeps the bearer token out of URLs.
        let store = TicketStore::new();
        let ticket = store.issue().expect("issues");
        let rendered = format!("{ticket:?}");
        assert_eq!(rendered, "Ticket(<redacted>)");
        assert!(!rendered.contains(ticket.as_str()));
    }
}
