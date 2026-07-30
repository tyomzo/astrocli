//! The command envelope: staleness rejection and idempotent retries — SDD §5.8.1's staleness
//! paragraph, §8.3(4).
//!
//! > Every state-changing request carries `issued_at` (client UTC) and a client-generated
//! > `command_id`. The server rejects motion-*initiating* commands whose `issued_at` is older than
//! > `max_command_age_ms` … **stopping commands are never staleness-rejected** — a late stop is
//! > safe, a late start is not. … `command_id` makes retries idempotent: a re-sent request with a
//! > known id returns the original outcome instead of re-executing.
//!
//! # The envelope travels in headers, not in the body
//!
//! §5.8.1 says the request *carries* the two values and does not say where, and its own route
//! table writes the bodies without them (`{ra_hours, dec_degrees}`, `{mode}`, `{axis?}`). Three
//! facts decide it:
//!
//! 1. **Half the mutation surface has no body to put it in.** `/api/mount/park`, `/unpark`,
//!    `/api/camera/capture/abort`, `/api/camera/fault/ack` and both live-view controls declare no
//!    body extractor at all, and `/api/mount/connect`, `/disconnect` and `/api/mount/slew/stop`
//!    take `Option<Json<…>>` precisely so a bare `POST` is not a 422. Putting the envelope in the
//!    body would make a JSON body *mandatory* on all of them — including on a stop, which is the
//!    one place this system refuses to grow a parse-failure path (§5.8.2, M1-T05).
//! 2. **It would have to be per-handler code.** Every request struct is `deny_unknown_fields`, so
//!    a body envelope means adding two fields to each of them and remembering on route eleven —
//!    the exact failure this task exists to prevent. Stripping them in middleware instead means
//!    buffering and re-serialising every command body on the control path.
//! 3. **A header is readable before the body is.** The check runs in a layer, ahead of the
//!    handler and ahead of any body extraction, which is what makes "the classification is
//!    unforgeable by a handler" true rather than customary.
//!
//! The response direction is a header for the same reason: §5.8.1 wants server time echoed in
//! *every* response, and a body field cannot be added to a `202` with no body or to a JPEG.
//!
//! # What is cached, and what is deliberately not
//!
//! The ledger caches the **HTTP response**, which is what makes §5.8.1's "returns the original
//! outcome" mean the right thing for `202 + WS progress` routes without anyone having to decide
//! what a goto's "outcome" is. A replayed goto answers the original `202 {correlation_id}` — the
//! answer that request actually got — and the slew it started is still reported on the event
//! stream under that same correlation id. Caching "the result of the command" one layer lower
//! would have had to choose between the 202 and a slew result that arrives minutes later.
//!
//! **Only 2xx outcomes are cached.** A command that failed did not complete, and a retry must
//! reach the device rather than be handed back the failure: replaying a cached `502
//! DEVICE_TRANSPORT` would turn one bad cable moment into a permanently refused command, and
//! replaying a cached `409 BUSY` would refuse a goto for five minutes after the mount went idle.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use astroctl_core::error::{ApiError, ErrorCode};
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};

use crate::route_meta::CommandClass;

// ---------------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------------

/// The client-generated command id (SDD §5.8.1's `command_id`).
pub const COMMAND_ID: &str = "astroctl-command-id";

/// When the operator's device believes it issued the command (§5.8.1's `issued_at`), RFC 3339.
pub const ISSUED_AT: &str = "astroctl-issued-at";

/// Server time on every response — §5.8.1's "echoing server time in every response".
pub const SERVER_TIME: &str = "astroctl-server-time";

/// `true` on a response served from the ledger rather than from the handler.
pub const REPLAYED: &str = "astroctl-replayed";

/// No `X-` prefix: RFC 6648 deprecated it in 2012, and these are not experimental.
fn header_name(name: &'static str) -> HeaderName {
    HeaderName::from_static(name)
}

// ---------------------------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------------------------

/// Entries retained, per SDD §5.8.1 as read by M1-T10 (~1024).
///
/// Sized against the failure it defends: an operator holding the D-pad renews the lease at 4 Hz,
/// so 1024 entries is four minutes of the busiest command stream this system can produce — and
/// the TTL below expires them before capacity ever becomes the binding constraint on a real
/// night. Capacity is here so a client with a broken id generator cannot grow the map without
/// bound.
pub const CAPACITY: usize = 1024;

/// How long an outcome stays replayable (5 min).
///
/// Long enough that a retry over a tunnel that stalled for minutes still finds its answer; short
/// enough that a `command_id` an operator's app reuses after a restart cannot resurrect an
/// answer from earlier in the night.
pub const TTL: Duration = Duration::from_secs(300);

/// The largest response body the ledger will retain.
///
/// Every mutation response in §5.8.1 is a small JSON object — a status, a `202 {correlation_id}`,
/// or the settings view, which is the biggest at roughly a kilobyte. The cap bounds the ledger at
/// `CAPACITY × 8 KiB` = 8 MiB in the worst case a hostile client could arrange, on a node whose
/// runtime is sized for a Raspberry Pi (SDD §7). A larger body is served normally and simply not
/// cached: idempotency is a courtesy to a retrying client, never a correctness requirement of the
/// response itself.
const MAX_CACHED_BODY: usize = 8 * 1024;

/// The largest body this layer will buffer at all. Beyond it the response cannot be reconstructed
/// and the request fails rather than being silently truncated.
const MAX_BUFFERED_BODY: usize = 1024 * 1024;

/// Shortest acceptable `command_id`.
///
/// A UUID is 36 characters and this codebase's own [`crate::ticket::Ticket`] is 43; eight is a
/// floor that rejects `"1"` and `"x"` — ids a hand-written client would collide on across two
/// browser tabs — without dictating a format.
const ID_MIN_LEN: usize = 8;

/// Longest acceptable `command_id`. The ledger stores every id it is given, so this is the term
/// that keeps [`CAPACITY`] a memory bound rather than a count.
const ID_MAX_LEN: usize = 128;

// ---------------------------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------------------------

/// One request's envelope, as read off the headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// The client's id for this command.
    pub id: String,
    /// When the client believes it issued the command, in UTC.
    pub issued_at: DateTime<Utc>,
}

impl Envelope {
    /// Read the envelope, naming the missing header when there is not one.
    ///
    /// # Errors
    /// [`ErrorCode::Validation`] (422) naming the header at fault. The message names it because
    /// the only caller that can ever see this is a client older than this node — the PWA was
    /// updated in the same change — and "422" with no field name sends its author reading the
    /// request body they did nothing wrong with.
    pub fn read(headers: &HeaderMap) -> Result<Self, ApiError> {
        let id = text(headers, COMMAND_ID)?;
        if id.len() < ID_MIN_LEN || id.len() > ID_MAX_LEN {
            return Err(ApiError::new(
                ErrorCode::Validation,
                format!(
                    "`{COMMAND_ID}` must be {ID_MIN_LEN}..={ID_MAX_LEN} characters; got {}",
                    id.len()
                ),
            ));
        }

        let raw = text(headers, ISSUED_AT)?;
        let issued_at = DateTime::parse_from_rfc3339(&raw)
            .map_err(|e| {
                ApiError::new(
                    ErrorCode::Validation,
                    format!("`{ISSUED_AT}` must be an RFC 3339 timestamp; got `{raw}` ({e})"),
                )
            })?
            .with_timezone(&Utc);

        Ok(Self { id, issued_at })
    }

    /// How far in the past this command was issued, by the node's clock.
    ///
    /// `None` when `issued_at` is in the *future*, which is skew rather than staleness: a client
    /// whose clock runs fast is exactly the case §5.8.1's skew correction exists for, and
    /// refusing it would turn a wrong clock into a telescope that cannot be driven. Only "older
    /// than" is a rejection, which is what the paragraph says.
    fn age(&self, now: DateTime<Utc>) -> Option<Duration> {
        (now - self.issued_at).to_std().ok()
    }
}

fn text(headers: &HeaderMap, name: &str) -> Result<String, ApiError> {
    let Some(value) = headers.get(name) else {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!(
                "missing `{name}`: every state-changing request carries `{COMMAND_ID}` and \
                 `{ISSUED_AT}` (SDD §5.8.1). This node's PWA sends both; a client that does not \
                 is older than the node it is talking to."
            ),
        ));
    };
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| ApiError::new(ErrorCode::Validation, format!("`{name}` is not valid text")))
}

// ---------------------------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------------------------

/// A completed command's answer, as it went out the first time.
#[derive(Clone, Debug)]
pub struct CachedOutcome {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl CachedOutcome {
    /// Rebuild the original response, marked as a replay.
    ///
    /// Two markers, because they answer to two readers. The [`REPLAYED`] header is the machine
    /// one and works for a body that is empty or is not JSON; `"replayed": true` inside a JSON
    /// object is the one an operator sees in `curl` and the one §5.8.1's "returns the original
    /// outcome" is checked against by hand. A body that is not a JSON object is passed through
    /// untouched rather than wrapped — rewriting it would make the replay a different document
    /// from the thing it claims to be.
    pub fn into_response(self) -> Response {
        let Self {
            status,
            mut headers,
            body,
        } = self;

        let body = mark_replayed(&body);
        headers.remove(axum::http::header::CONTENT_LENGTH);
        headers.insert(header_name(REPLAYED), HeaderValue::from_static("true"));

        let mut response = (status, body).into_response();
        // The handler's headers first, then whatever `into_response` derived (a content type for
        // the body we just rebuilt), so the rebuilt length wins over the recorded one.
        let derived = std::mem::replace(response.headers_mut(), headers);
        for (name, value) in &derived {
            response.headers_mut().insert(name, value.clone());
        }
        response
    }
}

/// Add `"replayed": true` to a JSON object body; leave anything else alone.
fn mark_replayed(body: &Bytes) -> Bytes {
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_slice(body) else {
        return body.clone();
    };
    map.insert("replayed".to_owned(), serde_json::Value::Bool(true));
    serde_json::to_vec(&serde_json::Value::Object(map)).map_or_else(|_| body.clone(), Bytes::from)
}

/// What the ledger holds for one `command_id`.
#[derive(Debug)]
enum Slot {
    /// A request with this id is executing right now.
    InFlight,
    /// It completed successfully; this is what it answered.
    Done { at: Instant, outcome: CachedOutcome },
}

#[derive(Debug, Default)]
struct Slots {
    by_id: HashMap<String, Slot>,
    /// Least-recently-used first — the eviction order when [`CAPACITY`] is reached.
    recency: VecDeque<String>,
}

impl Slots {
    fn touch(&mut self, id: &str) {
        if let Some(at) = self.recency.iter().position(|held| held == id) {
            self.recency.remove(at);
        }
        self.recency.push_back(id.to_owned());
    }

    fn forget(&mut self, id: &str) {
        self.by_id.remove(id);
        if let Some(at) = self.recency.iter().position(|held| held == id) {
            self.recency.remove(at);
        }
    }

    fn evict_to_capacity(&mut self) {
        while self.by_id.len() > CAPACITY {
            match self.recency.pop_front() {
                Some(oldest) => {
                    self.by_id.remove(&oldest);
                }
                // Unreachable while the two stay in step, and a `break` rather than an `unwrap`
                // because a bookkeeping slip must not panic a node that is driving a telescope.
                None => break,
            }
        }
    }
}

/// The per-node record of what has already been executed — SDD §5.8.1's idempotency, §8.3(4).
///
/// One per node, held in [`crate::api::AppState`] beside the ticket store: two nodes are two
/// command streams, and a process restart deliberately forgets — a `command_id` from before a
/// restart belongs to a session whose outcome the client can no longer act on anyway.
#[derive(Debug)]
pub struct CommandLedger {
    /// `server.max_command_age_ms` as a duration (SDD §5.8.1, default 2000).
    max_age: Duration,
    slots: Mutex<Slots>,
}

/// What the layer should do with a request, decided before the handler runs.
#[derive(Debug)]
pub enum Admission {
    /// Run the handler. The reservation, when present, must be settled with the response.
    Proceed(Option<Reservation>),
    /// Refuse it now.
    Refuse(ApiError),
    /// This exact command already completed; answer with what it answered.
    Replay(CachedOutcome),
}

impl CommandLedger {
    /// A ledger enforcing `max_command_age_ms`.
    #[must_use]
    pub fn new(max_age: Duration) -> Self {
        Self {
            max_age,
            slots: Mutex::new(Slots::default()),
        }
    }

    /// Decide what happens to one request, from its class and its headers.
    ///
    /// The class comes from [`crate::route_meta::RouteMeta`] and therefore from the route table,
    /// never from the request: a client that could nominate its own class could nominate
    /// `Stopping` for a goto and skip the age check entirely, which would leave §5.8.1's whole
    /// asymmetry decided by the caller it exists to constrain.
    pub fn admit(
        self: &Arc<Self>,
        class: CommandClass,
        headers: &HeaderMap,
        now: DateTime<Utc>,
    ) -> Admission {
        match class {
            // Not a command, or a command this node does not classify. Nothing is read, so a
            // stray header cannot change anything either.
            CommandClass::NotACommand | CommandClass::PassThrough | CommandClass::Exempt => {
                Admission::Proceed(None)
            }

            // A late stop is safe (§5.8.1), so *nothing* about the envelope may refuse one. A
            // missing or malformed envelope means the request runs without idempotency rather
            // than not running: the failure mode of an un-deduplicated stop is stopping something
            // that is already stopped, and the failure mode of a refused stop is a telescope
            // still moving.
            CommandClass::Stopping => match Envelope::read(headers) {
                Ok(envelope) => self.dedupe(&envelope),
                Err(_) => Admission::Proceed(None),
            },

            // Idempotent, not age-checked. See `CommandClass::Neutral` for what that buys.
            CommandClass::Neutral => match Envelope::read(headers) {
                Ok(envelope) => self.dedupe(&envelope),
                Err(error) => Admission::Refuse(error),
            },

            CommandClass::MotionInitiating => match Envelope::read(headers) {
                Ok(envelope) => {
                    // Deduplication first, and the order is the point: a hit means this command
                    // already executed, and staleness is a question about whether to *execute*.
                    // Refusing a replay as stale would deny a retrying client the answer to a
                    // command the node already carried out.
                    match self.dedupe(&envelope) {
                        Admission::Proceed(reservation) => {
                            match envelope.age(now) {
                                Some(age) if age > self.max_age => {
                                    // The reservation is dropped here, which releases it — a
                                    // refused command has not run and must not block its own id.
                                    drop(reservation);
                                    Admission::Refuse(stale(age, self.max_age))
                                }
                                _ => Admission::Proceed(reservation),
                            }
                        }
                        other => other,
                    }
                }
                Err(error) => Admission::Refuse(error),
            },
        }
    }

    /// Look the id up and, if it is new, reserve it.
    fn dedupe(self: &Arc<Self>, envelope: &Envelope) -> Admission {
        let mut slots = match self.slots.lock() {
            Ok(slots) => slots,
            // A poisoned mutex means a previous holder panicked. The ledger is a cache; carrying
            // on without deduplication is strictly better than refusing every command for the
            // remaining life of the process.
            Err(poisoned) => poisoned.into_inner(),
        };

        match slots.by_id.get(&envelope.id) {
            Some(Slot::Done { at, outcome }) if at.elapsed() < TTL => {
                let outcome = outcome.clone();
                slots.touch(&envelope.id);
                return Admission::Replay(outcome);
            }
            Some(Slot::Done { .. }) => {
                // Expired. Removed here rather than swept on a timer: the only moment anyone
                // cares whether an entry is alive is the moment it is asked for.
                slots.forget(&envelope.id);
            }
            Some(Slot::InFlight) => {
                return Admission::Refuse(ApiError::new(
                    ErrorCode::Busy,
                    format!(
                        "a command with `{COMMAND_ID}` `{}` is still executing; wait for its \
                         answer rather than re-sending it",
                        envelope.id
                    ),
                ));
            }
            None => {}
        }

        slots.by_id.insert(envelope.id.clone(), Slot::InFlight);
        slots.touch(&envelope.id);
        slots.evict_to_capacity();
        drop(slots);

        Admission::Proceed(Some(Reservation {
            ledger: Arc::clone(self),
            id: envelope.id.clone(),
            settled: false,
        }))
    }

    fn record(&self, id: &str, outcome: CachedOutcome) {
        let mut slots = match self.slots.lock() {
            Ok(slots) => slots,
            Err(poisoned) => poisoned.into_inner(),
        };
        slots.by_id.insert(
            id.to_owned(),
            Slot::Done {
                at: Instant::now(),
                outcome,
            },
        );
        slots.touch(id);
        slots.evict_to_capacity();
    }

    fn release(&self, id: &str) {
        let mut slots = match self.slots.lock() {
            Ok(slots) => slots,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Only an in-flight marker is released. A `Done` entry under the same id would be this
        // command's own recorded outcome, and dropping it would undo the record it just made.
        if matches!(slots.by_id.get(id), Some(Slot::InFlight)) {
            slots.forget(id);
        }
    }

    /// Entries currently held — for tests and for nothing else.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.lock().map_or(0, |slots| slots.by_id.len())
    }
}

fn stale(age: Duration, max_age: Duration) -> ApiError {
    ApiError::new(
        ErrorCode::CommandStale,
        format!(
            "this command was issued {} ms ago and motion-initiating commands expire after {} ms \
             (SDD §5.8.1): a request delayed this long must not start motion after the operator's \
             intent has passed. Re-issue it.",
            age.as_millis(),
            max_age.as_millis()
        ),
    )
}

/// A `command_id` held for the duration of one execution.
///
/// The `Drop` impl is not tidiness, it is the reason the in-flight marker is safe to take at all.
/// A client that gives up mid-request — the flaky tunnel this whole mechanism is about — makes
/// axum drop the handler future, and a marker leaked there would refuse that client's retry for
/// five minutes with a `409` about a command that is no longer running.
#[derive(Debug)]
pub struct Reservation {
    ledger: Arc<CommandLedger>,
    id: String,
    settled: bool,
}

impl Reservation {
    /// Record the response under this id if it succeeded, and hand it back.
    ///
    /// Buffers the body, which is what makes a replay possible at all. Only routes carrying an
    /// envelope reach here, so nothing streamed — the preview JPEG, the WS upgrade — is ever
    /// buffered by this path.
    pub async fn settle(mut self, response: Response) -> Response {
        let status = response.status();
        let (parts, body) = response.into_parts();

        let Ok(bytes) = axum::body::to_bytes(body, MAX_BUFFERED_BODY).await else {
            self.ledger.release(&self.id);
            self.settled = true;
            return ApiError::new(
                ErrorCode::Internal,
                "the response body could not be read back for the command ledger",
            )
            .pipe_failure();
        };

        if status.is_success() && bytes.len() <= MAX_CACHED_BODY {
            self.ledger.record(
                &self.id,
                CachedOutcome {
                    status,
                    headers: parts.headers.clone(),
                    body: bytes.clone(),
                },
            );
        } else {
            // Not cached: a failure did not complete, and an oversized body is not worth the
            // memory. Either way the id must not stay marked in-flight.
            self.ledger.release(&self.id);
        }
        self.settled = true;

        Response::from_parts(parts, axum::body::Body::from(bytes))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            self.ledger.release(&self.id);
        }
    }
}

/// Small helper so [`Reservation::settle`] can return an error response without importing the
/// API layer's wrapper into this module's signature.
trait PipeFailure {
    fn pipe_failure(self) -> Response;
}

impl PipeFailure for ApiError {
    fn pipe_failure(self) -> Response {
        crate::api::ApiFailure(self).into_response()
    }
}

/// The one spelling of a timestamp on this surface — SDD §2's UTC, milliseconds, `Z`.
///
/// The same `SecondsFormat::Millis` the event schema serialises with (§4.3), so a client parsing
/// `astroctl-server-time` and a client parsing an event's `ts` are parsing the same shape.
fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Stamp the node's clock on a response — §5.8.1's "echoing server time in every response".
///
/// Every response, not only a command's: the value's whole job is to let a client measure its own
/// skew, and a client that has not issued a command yet is exactly the one that needs to know.
pub fn stamp_server_time(response: &mut Response, now: DateTime<Utc>) {
    if let Ok(value) = HeaderValue::from_str(&rfc3339(now)) {
        response
            .headers_mut()
            .insert(header_name(SERVER_TIME), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn ledger() -> Arc<CommandLedger> {
        Arc::new(CommandLedger::new(Duration::from_millis(2000)))
    }

    fn headers(id: &str, issued_at: DateTime<Utc>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header_name(COMMAND_ID),
            HeaderValue::from_str(id).expect("ascii"),
        );
        headers.insert(
            header_name(ISSUED_AT),
            HeaderValue::from_str(&rfc3339(issued_at)).expect("ascii"),
        );
        headers
    }

    fn json_response(status: StatusCode, body: &str) -> Response {
        Response::builder()
            .status(status)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .expect("response builds")
    }

    async fn body_of(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // --- reading the envelope -------------------------------------------------------------

    #[test]
    fn a_missing_header_is_named_in_the_refusal() {
        let error = Envelope::read(&HeaderMap::new()).expect_err("no headers at all");
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains(COMMAND_ID), "{}", error.message);
        assert_eq!(error.http_status(), 422);

        let mut only_id = HeaderMap::new();
        only_id.insert(
            header_name(COMMAND_ID),
            HeaderValue::from_static("abcdefgh12"),
        );
        let error = Envelope::read(&only_id).expect_err("no timestamp");
        assert!(error.message.contains(ISSUED_AT), "{}", error.message);
    }

    #[test]
    fn a_timestamp_that_is_not_rfc_3339_names_the_header_and_the_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header_name(COMMAND_ID),
            HeaderValue::from_static("abcdefgh12"),
        );
        headers.insert(
            header_name(ISSUED_AT),
            HeaderValue::from_static("last tuesday"),
        );
        let error = Envelope::read(&headers).expect_err("not a timestamp");
        assert!(error.message.contains(ISSUED_AT), "{}", error.message);
        assert!(error.message.contains("last tuesday"), "{}", error.message);
    }

    #[test]
    fn an_id_outside_the_length_bounds_is_refused() {
        for id in ["short", &"x".repeat(ID_MAX_LEN + 1)] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header_name(COMMAND_ID),
                HeaderValue::from_str(id).expect("ascii"),
            );
            let error = Envelope::read(&headers).expect_err("bad length");
            assert!(error.message.contains(COMMAND_ID), "{}", error.message);
        }
    }

    /// The client's clock running *fast* is the case §5.8.1's skew correction exists for. A
    /// future `issued_at` is therefore never a staleness rejection — it is the symptom the
    /// mechanism is meant to survive.
    #[test]
    fn a_future_issued_at_has_no_age_and_so_cannot_be_stale() {
        let now = Utc::now();
        let envelope = Envelope {
            id: "abcdefgh12".to_owned(),
            issued_at: now + chrono::Duration::seconds(60),
        };
        assert_eq!(envelope.age(now), None);

        let admission = ledger().admit(
            CommandClass::MotionInitiating,
            &headers("abcdefgh12", now + chrono::Duration::seconds(60)),
            now,
        );
        assert!(
            matches!(admission, Admission::Proceed(Some(_))),
            "{admission:?}"
        );
    }

    // --- staleness ------------------------------------------------------------------------

    #[tokio::test]
    async fn a_five_second_old_motion_command_is_refused_and_a_fresh_one_is_not() {
        let now = Utc::now();
        let ledger = ledger();

        let stale = ledger.admit(
            CommandClass::MotionInitiating,
            &headers("stale-command-id", now - chrono::Duration::seconds(5)),
            now,
        );
        let Admission::Refuse(error) = stale else {
            panic!("a 5 s old goto must be refused: {stale:?}");
        };
        assert_eq!(error.code, ErrorCode::CommandStale);
        assert_eq!(error.http_status(), 422);

        let fresh = ledger.admit(
            CommandClass::MotionInitiating,
            &headers("fresh-command-id", now),
            now,
        );
        assert!(matches!(fresh, Admission::Proceed(Some(_))), "{fresh:?}");
    }

    /// The refused command never ran, so its id must be free again — otherwise the client's
    /// re-issue with a fresh timestamp would collide with its own rejection.
    #[test]
    fn a_stale_refusal_does_not_hold_the_id() {
        let now = Utc::now();
        let ledger = ledger();
        let _ = ledger.admit(
            CommandClass::MotionInitiating,
            &headers("stale-command-id", now - chrono::Duration::seconds(5)),
            now,
        );
        assert_eq!(ledger.len(), 0, "a refused command holds no reservation");
    }

    /// The §5.8.1 asymmetry, as an assertion: the same age, the same headers, two classes.
    #[test]
    fn a_stop_of_the_same_age_is_honoured() {
        let now = Utc::now();
        let admission = ledger().admit(
            CommandClass::Stopping,
            &headers("old-stop-command", now - chrono::Duration::seconds(5)),
            now,
        );
        assert!(
            matches!(admission, Admission::Proceed(Some(_))),
            "{admission:?}"
        );
    }

    /// And an hour-old stop, so the test is about the rule rather than about 5 being under some
    /// other threshold.
    #[test]
    fn no_age_at_all_can_refuse_a_stop() {
        let now = Utc::now();
        let admission = ledger().admit(
            CommandClass::Stopping,
            &headers("ancient-stop-cmd", now - chrono::Duration::hours(1)),
            now,
        );
        assert!(
            matches!(admission, Admission::Proceed(Some(_))),
            "{admission:?}"
        );
    }

    /// A stop with no envelope at all still runs. This is the property that keeps a stopping
    /// route free of the 422 path §5.8.2 exists to prevent.
    #[test]
    fn a_stop_without_an_envelope_is_not_refused() {
        let admission = ledger().admit(CommandClass::Stopping, &HeaderMap::new(), Utc::now());
        assert!(
            matches!(admission, Admission::Proceed(None)),
            "{admission:?}"
        );
    }

    #[test]
    fn a_neutral_mutation_needs_an_envelope_but_not_a_fresh_one() {
        let now = Utc::now();
        let ledger = ledger();

        let missing = ledger.admit(CommandClass::Neutral, &HeaderMap::new(), now);
        let Admission::Refuse(error) = missing else {
            panic!("a neutral mutation without an envelope must be refused: {missing:?}");
        };
        assert_eq!(error.code, ErrorCode::Validation);

        let old = ledger.admit(
            CommandClass::Neutral,
            &headers("old-settings-put", now - chrono::Duration::hours(1)),
            now,
        );
        assert!(
            matches!(old, Admission::Proceed(Some(_))),
            "a settings change is a state, not an event, and does not expire: {old:?}"
        );
    }

    #[test]
    fn the_exempt_and_uncovered_classes_read_nothing() {
        for class in [
            CommandClass::Exempt,
            CommandClass::NotACommand,
            CommandClass::PassThrough,
        ] {
            let admission = ledger().admit(class, &HeaderMap::new(), Utc::now());
            assert!(
                matches!(admission, Admission::Proceed(None)),
                "{class:?}: {admission:?}"
            );
        }
    }

    // --- idempotency ----------------------------------------------------------------------

    #[tokio::test]
    async fn a_repeated_command_id_replays_the_original_outcome_marked_as_a_replay() {
        let now = Utc::now();
        let ledger = ledger();
        let headers = headers("goto-command-001", now);

        let Admission::Proceed(Some(reservation)) =
            ledger.admit(CommandClass::MotionInitiating, &headers, now)
        else {
            panic!("first call must proceed");
        };
        let first = reservation
            .settle(json_response(
                StatusCode::ACCEPTED,
                r#"{"correlation_id":"abc","watch_topic":"mount.position"}"#,
            ))
            .await;
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        assert!(first.headers().get(REPLAYED).is_none());

        let Admission::Replay(outcome) =
            ledger.admit(CommandClass::MotionInitiating, &headers, now)
        else {
            panic!("second call with the same id must replay");
        };
        let second = outcome.into_response();
        assert_eq!(
            second.status(),
            StatusCode::ACCEPTED,
            "the outcome to replay is the 202, not the slew's eventual result"
        );
        assert_eq!(second.headers().get(REPLAYED).expect("marker"), "true");

        let body: serde_json::Value =
            serde_json::from_str(&body_of(second).await).expect("json object");
        assert_eq!(body["replayed"], true);
        assert_eq!(
            body["correlation_id"], "abc",
            "the correlation id is the whole value of replaying a 202"
        );
    }

    /// A failure is not an outcome to replay: the command did not complete, and a retry must
    /// reach the device.
    #[tokio::test]
    async fn a_failed_command_is_not_cached() {
        let now = Utc::now();
        let ledger = ledger();
        let headers = headers("failing-command1", now);

        let Admission::Proceed(Some(reservation)) =
            ledger.admit(CommandClass::Neutral, &headers, now)
        else {
            panic!("first call must proceed");
        };
        let _ = reservation
            .settle(json_response(
                StatusCode::BAD_GATEWAY,
                r#"{"code":"DEVICE_TRANSPORT"}"#,
            ))
            .await;

        let retry = ledger.admit(CommandClass::Neutral, &headers, now);
        assert!(
            matches!(retry, Admission::Proceed(Some(_))),
            "a retry after a 502 must reach the device: {retry:?}"
        );
    }

    #[test]
    fn a_concurrent_duplicate_is_refused_rather_than_executed_twice() {
        let now = Utc::now();
        let ledger = ledger();
        let headers = headers("inflight-command", now);

        let Admission::Proceed(Some(_held)) = ledger.admit(CommandClass::Neutral, &headers, now)
        else {
            panic!("first call must proceed");
        };
        let second = ledger.admit(CommandClass::Neutral, &headers, now);
        let Admission::Refuse(error) = second else {
            panic!("a duplicate arriving mid-execution must not execute: {second:?}");
        };
        assert_eq!(error.code, ErrorCode::Busy);
    }

    /// The flaky-tunnel case: the client gave up and the handler future was dropped. The id has
    /// to be free again or the retry meets a 409 about a command nothing is running.
    #[test]
    fn dropping_a_reservation_unheld_releases_the_id() {
        let now = Utc::now();
        let ledger = ledger();
        let headers = headers("abandoned-comm1", now);

        let Admission::Proceed(Some(reservation)) =
            ledger.admit(CommandClass::Neutral, &headers, now)
        else {
            panic!("first call must proceed");
        };
        drop(reservation);
        assert_eq!(ledger.len(), 0);

        let retry = ledger.admit(CommandClass::Neutral, &headers, now);
        assert!(matches!(retry, Admission::Proceed(Some(_))), "{retry:?}");
    }

    #[tokio::test]
    async fn the_ledger_is_bounded_at_capacity() {
        let now = Utc::now();
        let ledger = ledger();
        for n in 0..(CAPACITY + 50) {
            let headers = headers(&format!("command-id-{n:08}"), now);
            let Admission::Proceed(Some(reservation)) =
                ledger.admit(CommandClass::Neutral, &headers, now)
            else {
                panic!("call {n} must proceed");
            };
            let _ = reservation
                .settle(json_response(StatusCode::OK, "{}"))
                .await;
        }
        assert_eq!(ledger.len(), CAPACITY);

        // The oldest ids were evicted, so their retries execute again rather than replaying.
        let oldest = headers("command-id-00000000", now);
        assert!(
            matches!(
                ledger.admit(CommandClass::Neutral, &oldest, now),
                Admission::Proceed(Some(_))
            ),
            "an evicted id is a new command"
        );
    }

    #[tokio::test]
    async fn an_oversized_body_is_served_but_not_cached() {
        let now = Utc::now();
        let ledger = ledger();
        let headers = headers("huge-response-1", now);

        let Admission::Proceed(Some(reservation)) =
            ledger.admit(CommandClass::Neutral, &headers, now)
        else {
            panic!("first call must proceed");
        };
        let big = "x".repeat(MAX_CACHED_BODY + 1);
        let served = reservation
            .settle(json_response(StatusCode::OK, &big))
            .await;
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(body_of(served).await.len(), big.len());
        assert_eq!(ledger.len(), 0, "nothing that large is retained");
    }

    /// A body that is not a JSON object keeps its bytes. The header is what a client reads in
    /// that case.
    #[tokio::test]
    async fn a_non_object_body_is_replayed_verbatim() {
        let now = Utc::now();
        let ledger = ledger();
        let headers = headers("empty-body-cmd1", now);

        let Admission::Proceed(Some(reservation)) =
            ledger.admit(CommandClass::Neutral, &headers, now)
        else {
            panic!("first call must proceed");
        };
        let _ = reservation
            .settle(
                Response::builder()
                    .status(StatusCode::ACCEPTED)
                    .body(Body::empty())
                    .expect("builds"),
            )
            .await;

        let Admission::Replay(outcome) = ledger.admit(CommandClass::Neutral, &headers, now) else {
            panic!("must replay");
        };
        let replayed = outcome.into_response();
        assert_eq!(replayed.status(), StatusCode::ACCEPTED);
        assert_eq!(replayed.headers().get(REPLAYED).expect("marker"), "true");
        assert_eq!(body_of(replayed).await, "");
    }

    #[test]
    fn server_time_is_rfc_3339_utc() {
        let mut response = json_response(StatusCode::OK, "{}");
        let now = Utc::now();
        stamp_server_time(&mut response, now);
        let stamped = response
            .headers()
            .get(SERVER_TIME)
            .expect("stamped")
            .to_str()
            .expect("ascii");
        assert!(stamped.ends_with('Z'), "SDD §2 wants UTC: {stamped}");
        assert_eq!(
            DateTime::parse_from_rfc3339(stamped)
                .expect("parses")
                .timestamp_millis(),
            now.timestamp_millis()
        );
    }
}
