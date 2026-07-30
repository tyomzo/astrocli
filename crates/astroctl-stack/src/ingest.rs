//! `POST /api/ingest` and `GET /api/stacking/stats` — SDD §5.11.1, §5.11.2.
//!
//! The receiving half of ADR-05, and the half that has to be paranoid: the field node is
//! entitled to retry blindly, so every answer this module gives has to be safe to give twice.
//!
//! # What an answer means to the sender
//!
//! SDD §5.10.1 makes the transfer agent's failure handling depend on the *status class*, not on
//! the message: a 4xx that is not 408/429 sends a frame to the terminal `failed` state and stops
//! the retries; anything else is retried with backoff. Every response below is chosen against
//! that rule, because getting it wrong either abandons a good frame or retries a hopeless one
//! forever:
//!
//! | Situation | Answer | Why |
//! |-----------|--------|-----|
//! | stored, or already stored | `200` | the bytes are on disk, fsynced, checksum matched |
//! | checksum or size disagrees with the metadata | `422 CHECKSUM_MISMATCH` | definitive: the same bytes will fail again |
//! | id taken by different bytes | `409 FRAME_ID_CONFLICT` | definitive, and REL-11 forbids the overwrite |
//! | malformed request, or over the frame ceiling | `422 VALIDATION` | definitive |
//! | free space below the critical threshold | `507 DISK_FULL` | *not* definitive — the operator frees space and the identical request succeeds (REL-12) |
//! | the body stopped arriving | `5xx` | not the sender's fault and not definitive |
//!
//! # Why a duplicate is drained before it is acked
//!
//! The tempting optimization is to answer `200 {duplicate: true}` the moment the metadata part
//! identifies a frame we already hold, without reading the 25 MB behind it. It does not work.
//! Responding before the body is consumed makes hyper close the connection, and a client still
//! writing gets a transport error rather than the response — so the frame would never be marked
//! acked and would be retried forever. The body is therefore drained first for every *definitive*
//! answer. A 507 is the deliberate exception: it means "come back later", the sender requeues on
//! a transport error exactly as it would on the 507, and making a node whose disk is full accept
//! 25 MB before saying so would defeat the point of REL-12's back-pressure.
//!
//! The cost is a retransmission the sender cannot avoid anyway — it has already begun sending by
//! the time we know. The way to actually save it is a cheap pre-flight the sender can make
//! before it commits to a body; see the M1-T12 result note.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};

use astroctl_core::error::{ApiError, ErrorCode};
use astroctl_core::event::{ts_rfc3339_millis_opt, Alert};
use axum::extract::{ConnectInfo, FromRequestParts, Multipart, Path, State};
use axum::http::request::Parts;
use axum::http::{header::HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{ApiFailure, AppState};
use crate::journal::JournalError;
use crate::mirror::{FrameRef, MirrorError, Outcome, SessionMeta, SessionMirror, StageError};
use crate::vitals;

/// Schema version of the ingest metadata and of the ack (SDD §2).
///
/// Checked for equality, not for a compatible range — the same decision as the worker handshake
/// of SDD §5.12.2, and for the same reason: two nodes shipped from one repository disagreeing
/// about the wire format is a deployment mistake, and a mistake is better refused loudly on the
/// first frame than absorbed silently on all of them.
pub const INGEST_SCHEMA_VERSION: u16 = 1;

/// The metadata part of the multipart body.
const META_PART: &str = "meta";

/// The frame part of the multipart body.
const FRAME_PART: &str = "frame";

/// The largest single frame this node will accept.
///
/// A Canon CR3 is ~25 MB and an uncompressed full-frame FITS runs to a few hundred; 512 MiB is
/// well clear of both while still being a number, so a client that declares a nonsensical size is
/// refused before anything is written. This is a ceiling, not a policy: the declared size in the
/// metadata is what actually bounds each transfer, and it is enforced per chunk.
pub const MAX_FRAME_BYTES: u64 = 512 * 1024 * 1024;

/// Body limit for the ingest route, replacing axum's 2 MiB default.
///
/// The frame ceiling plus room for the metadata part and the MIME framing around both. Without
/// this the very first 25 MB upload is rejected with a 413 that no code in SDD §4.2 describes.
pub const MAX_UPLOAD_BYTES: usize = (MAX_FRAME_BYTES + (1 << 20)) as usize;

// ---------------------------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------------------------

/// The `meta` part: everything about a frame except its bytes.
///
/// `deny_unknown_fields` because both nodes ship from one repository at one version: a field the
/// stack node does not recognize is a typo or a version skew, and silently dropping it would mean
/// losing capture metadata with no symptom. `v` is what makes that strictness safe — a real
/// protocol change announces itself instead of arriving as an unknown field.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameMeta {
    /// Schema version; must equal [`INGEST_SCHEMA_VERSION`].
    pub v: u16,
    /// Session the frame belongs to, mirrored as `sessions/<session_id>/`.
    pub session_id: String,
    /// Frame id as the field node assigned it, e.g. `light_00042` (SDD §5.5).
    pub frame_id: String,
    /// Lowercase hex SHA-256 of the frame bytes, as computed by the field node.
    pub sha256: String,
    /// Exact size of the frame in bytes.
    pub size: u64,
    /// Extension of the raw, without the dot, e.g. `cr3`.
    ///
    /// Explicit rather than taken from the multipart part's `filename`: the stored path is then
    /// derived entirely from validated metadata, and a `filename` is the classic place a
    /// traversal arrives from.
    pub ext: String,
    /// Capture parameters, mirrored verbatim into `control/quality_<id>.json`.
    #[serde(default)]
    pub capture: Option<serde_json::Value>,
    /// Session-level metadata, from which `session.json` is constructed (SDD §5.11.3).
    #[serde(default)]
    pub session: Option<SessionMeta>,
}

/// The ack of SDD §5.11.1.
///
/// Carries `v` (SDD §2 requires it of every externally visible schema, and the route table's
/// sketch omits it) and echoes the identity as well as the checksum, so a sender that pipelines
/// can tell which frame an ack belongs to rather than assuming the order it sent them in.
#[derive(Clone, Debug, Serialize)]
pub struct IngestAck {
    /// Schema version.
    pub v: u16,
    /// Session the frame was filed under.
    pub session_id: String,
    /// Frame id, echoed.
    pub frame_id: String,
    /// Checksum of the bytes now on this node's disk. SDD §5.10.2 has the sender verify this
    /// against its own before marking the frame acked.
    pub sha256: String,
    /// Always `true` on a 200: the bytes are here, fsynced.
    pub stored: bool,
    /// Whether this node already held these bytes, in which case nothing was written.
    pub duplicate: bool,
}

/// `GET /api/stacking/stats` — SDD §5.11.1. Real statistics arrive in Phase 2b.
#[derive(Clone, Debug, Serialize)]
pub struct StatsResponse {
    /// Schema version.
    pub v: u16,
    /// The session that most recently received a frame; `null` on a node that holds none.
    pub session_id: Option<String>,
    /// Frames held for that session.
    pub frame_count: u64,
    /// When its last frame arrived.
    #[serde(with = "ts_rfc3339_millis_opt")]
    pub last_ingest_ts: Option<DateTime<Utc>>,
    /// When this node last produced a preview; `null` on a node that has produced none.
    ///
    /// Filled by M1-T14's preview pipeline. It is deliberately **not** per-session: it answers
    /// "is the stack still turning frames into pictures", which is a question about the node, and
    /// the field node forwards it into `stack.status` for exactly that (§4.3, USB-06).
    #[serde(with = "ts_rfc3339_millis_opt")]
    pub last_preview_ts: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

/// Ingest's slice of the application state.
pub struct Ingest {
    archive: SessionMirror,
    /// Whether the last decision was to refuse for space, so the REL-12 alert is edge-triggered.
    ///
    /// SDD §5.10.4's rule — one alert on the transition, never one per attempt — applies here
    /// even harder than it does to the watchdog: a backed-up field node retries continuously, and
    /// an alert per refused frame is an alert the operator learns to ignore.
    refusing_for_space: AtomicBool,
}

impl Ingest {
    /// Wrap an opened archive.
    #[must_use]
    pub const fn new(archive: SessionMirror) -> Self {
        Self {
            archive,
            refusing_for_space: AtomicBool::new(false),
        }
    }

    /// The mirrored archive.
    #[must_use]
    pub const fn archive(&self) -> &SessionMirror {
        &self.archive
    }
}

/// Who uploaded, for the journal's `source` column.
///
/// Behind the field node's `/stack/*` proxy (ADR-07) this is the field node rather than the
/// original client, which is the useful answer while there is one uploader: it distinguishes the
/// normal path from someone posting to this node directly. `None` when the connection info is not
/// available, which is honest rather than a fabricated address.
#[derive(Clone, Copy, Debug)]
pub struct Source(Option<SocketAddr>);

impl Source {
    fn label(self) -> Option<String> {
        self.0.map(|addr| addr.to_string())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Source {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `HEAD`/`GET /api/ingest/{session_id}/{frame_id}` — the pre-flight of SDD §5.11.1.
///
/// M1-T11 measured the cost this removes: without it, a duplicate is discovered only after the
/// full body has crossed the link — ~200 s for a 48 MB frame at the T-HOL-1 1 Mbit shape — and
/// the sender learns `duplicate: true` about bytes the far side already held. The sender asks
/// this first and treats *any* non-204 answer as "not stored, upload", so the route is an
/// optimisation and never a gate — a node without it (or a 500 from it) costs a body, not a
/// frame.
///
/// Declared as GET and served for HEAD too (axum answers HEAD from a GET handler, dropping the
/// body — a 204 has none anyway). The stored hash rides a header so a HEAD can carry it.
pub async fn preflight(
    State(state): State<AppState>,
    Path((session_id, frame_id)): Path<(String, String)>,
) -> Result<Response, ApiFailure> {
    let stored = state
        .ingest
        .archive()
        .journal()
        .lookup(&session_id, &frame_id)
        .await
        .map_err(ApiFailure::from)?;

    match stored {
        Some(frame) => Ok((
            StatusCode::NO_CONTENT,
            [(
                HeaderName::from_static("x-astroctl-sha256"),
                frame.sha256.clone(),
            )],
        )
            .into_response()),
        // The path is not echoed: it is caller-controlled text (same reasoning as pwa::api_miss).
        None => Err(ApiFailure(ApiError::new(
            ErrorCode::NotFound,
            "that frame is not stored on this node",
        ))),
    }
}

/// `POST /api/ingest` — the procedure of SDD §5.11.2.
pub async fn ingest(
    State(state): State<AppState>,
    source: Source,
    mut multipart: Multipart,
) -> Result<Json<IngestAck>, ApiFailure> {
    let meta = read_meta(&mut multipart).await.map_err(ApiFailure)?;

    // REL-12, before a byte of the frame is read — see the module docs for why this one answer
    // short-circuits rather than draining.
    if let Some(refusal) = refuse_for_space(&state) {
        return Err(ApiFailure(refusal));
    }

    let frame = FrameRef {
        session_id: &meta.session_id,
        frame_id: &meta.frame_id,
        ext: &meta.ext,
    };
    let archive = state.ingest.archive();

    // The dedup fast path of SDD §5.11.2: already stored, so the file is never touched.
    if let Some(stored) = archive.lookup(&meta.session_id, &meta.frame_id).await? {
        drain(&mut multipart).await;
        return if stored.sha256 == meta.sha256 {
            tracing::debug!(
                session_id = %meta.session_id,
                frame_id = %meta.frame_id,
                "re-upload of a frame already stored; acked without rewriting it"
            );
            Ok(Json(ack(&meta, true)))
        } else {
            Err(ApiFailure(conflict(&state, &meta, &stored.sha256)))
        };
    }

    let mut field = frame_part(&mut multipart).await.map_err(ApiFailure)?;
    let mut staging = archive.stage(frame, meta.size).await?;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if let Err(error) = staging.write(&chunk).await {
                    staging.abort().await;
                    return Err(ApiFailure(match error {
                        // A body longer than its metadata says is the same class of failure as a
                        // wrong checksum — the bytes are not what was described — and definitive
                        // for the same reason.
                        StageError::TooLarge { limit } => ApiError::new(
                            ErrorCode::ChecksumMismatch,
                            format!(
                                "the frame body is longer than the {limit} bytes its metadata \
                                 declares"
                            ),
                        ),
                        StageError::Mirror(error) => ApiError::from(error),
                    }));
                }
            }
            Ok(None) => break,
            Err(error) => {
                staging.abort().await;
                return Err(ApiFailure(from_multipart(&error)));
            }
        }
    }

    let verified = staging.finish().await?;
    if verified.sha256() != meta.sha256 || verified.size_bytes() != meta.size {
        let (got, size) = (verified.sha256().to_owned(), verified.size_bytes());
        verified.discard().await;
        return Err(ApiFailure(checksum_mismatch(&state, &meta, &got, size)));
    }

    match archive
        .commit(
            frame,
            verified,
            meta.capture.clone(),
            meta.session.clone(),
            source.label(),
        )
        .await?
    {
        Outcome::Stored(counts) => {
            accepting_again(&state);
            tracing::info!(
                session_id = %meta.session_id,
                frame_id = %meta.frame_id,
                size_bytes = meta.size,
                frame_count = counts.frame_count,
                "frame ingested"
            );
            // The preview is submitted **here**: after the journal row, on the path that is about
            // to ack. Two properties of that placement are load-bearing.
            //
            // *After the row*, because the row is REL-13's authority — the thing a field node
            // deletes its only copy on the strength of — and a preview must never be able to
            // reorder itself in front of it.
            //
            // *Infallibly*, because §5.12.3 is explicit that "ingest acks on durability, not on
            // processing". `offer` cannot fail, cannot block and cannot await, so there is no
            // path by which a sick worker turns a stored frame into a 5xx and a retransmission.
            // A preview that never happens costs the operator an image they can get by capturing
            // again; an ingest that fails over one costs 25 MB of a shaped link.
            state
                .preview_queue
                .offer(&meta.session_id, &meta.frame_id, archive.frame_path(frame));
            Ok(Json(ack(&meta, false)))
        }
        Outcome::Duplicate(_) => {
            accepting_again(&state);
            Ok(Json(ack(&meta, true)))
        }
        Outcome::Conflict { stored_sha256 } => {
            Err(ApiFailure(conflict(&state, &meta, &stored_sha256)))
        }
    }
}

/// `GET /api/stacking/stats` — SDD §5.11.1.
pub async fn stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, ApiFailure> {
    let latest = state.ingest.archive().journal().latest_session().await?;
    Ok(Json(StatsResponse {
        v: INGEST_SCHEMA_VERSION,
        session_id: latest.as_ref().map(|s| s.session_id.clone()),
        frame_count: latest.as_ref().map_or(0, |s| s.frame_count),
        last_ingest_ts: latest.as_ref().map(|s| s.last_ingest_ts),
        last_preview_ts: state.previews.last_preview_ts(),
    }))
}

// ---------------------------------------------------------------------------------------------
// Parts
// ---------------------------------------------------------------------------------------------

/// Read and validate the `meta` part, which must arrive first.
///
/// Ordering is a requirement, not a convention: the destination path, the declared size and the
/// dedup answer all come out of this part, and a frame cannot be streamed anywhere until they
/// are known. Buffering the frame instead so the parts could arrive in any order would put 25 MB
/// in RAM, which SDD §5.11.2 rules out in the same sentence that defines the procedure.
async fn read_meta(multipart: &mut Multipart) -> Result<FrameMeta, ApiError> {
    let field = multipart
        .next_field()
        .await
        .map_err(|e| from_multipart(&e))?
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::Validation,
                format!("the request body has no `{META_PART}` part"),
            )
        })?;

    let name = field.name().unwrap_or_default().to_owned();
    if name != META_PART {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!("the first part of the body must be `{META_PART}`, not `{name}`"),
        ));
    }

    let text = field.text().await.map_err(|e| from_multipart(&e))?;
    let mut meta: FrameMeta = serde_json::from_str(&text).map_err(|error| {
        ApiError::new(
            ErrorCode::Validation,
            format!("the `{META_PART}` part is not valid ingest metadata: {error}"),
        )
    })?;

    // `frame.saved` already lowercases (SDD §4.3); accepting either spelling here means an ack
    // can never fail the sender's echo comparison over case alone.
    meta.sha256.make_ascii_lowercase();
    validate(&meta)?;
    Ok(meta)
}

/// Take the `frame` part, which must follow `meta`.
async fn frame_part<'a>(
    multipart: &'a mut Multipart,
) -> Result<axum::extract::multipart::Field<'a>, ApiError> {
    let field = multipart
        .next_field()
        .await
        .map_err(|e| from_multipart(&e))?
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::Validation,
                format!("the request body has no `{FRAME_PART}` part"),
            )
        })?;

    let name = field.name().unwrap_or_default().to_owned();
    if name != FRAME_PART {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!("the second part of the body must be `{FRAME_PART}`, not `{name}`"),
        ));
    }
    Ok(field)
}

/// Read the rest of the body and throw it away — see the module docs.
async fn drain(multipart: &mut Multipart) {
    while let Ok(Some(mut field)) = multipart.next_field().await {
        while let Ok(Some(_)) = field.chunk().await {}
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Every constraint the stored path and the journal key depend on.
///
/// These are not cosmetic. `session_id` and `frame_id` become directory and file names, so a `/`
/// or a `..` here is a write outside the archive; the checks are whitelists rather than
/// blacklists for that reason.
fn validate(meta: &FrameMeta) -> Result<(), ApiError> {
    if meta.v != INGEST_SCHEMA_VERSION {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!(
                "ingest metadata version {} is not the {INGEST_SCHEMA_VERSION} this node speaks",
                meta.v
            ),
        ));
    }
    if !is_session_id(&meta.session_id) {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!(
                "`session_id` must be 1-64 characters of [A-Za-z0-9._-] and may not begin with \
                 a dot: {:?}",
                meta.session_id
            ),
        ));
    }
    if !is_frame_id(&meta.frame_id) {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!(
                "`frame_id` must have the `<kind>_<id>` shape of SDD §5.5, e.g. `light_00042`: \
                 {:?}",
                meta.frame_id
            ),
        ));
    }
    if !is_extension(&meta.ext) {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!("`ext` must be 1-8 lowercase alphanumerics: {:?}", meta.ext),
        ));
    }
    if !is_sha256(&meta.sha256) {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!("`sha256` must be 64 hex digits: {:?}", meta.sha256),
        ));
    }
    if meta.size == 0 || meta.size > MAX_FRAME_BYTES {
        return Err(ApiError::new(
            ErrorCode::Validation,
            format!(
                "`size` must be between 1 and {MAX_FRAME_BYTES} bytes: {}",
                meta.size
            ),
        ));
    }
    Ok(())
}

fn is_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `<kind>_<id>`, e.g. `light_00042` — the shape SDD §5.5 assigns and §5.11.3 mirrors. A dot is
/// excluded so the stored name has exactly one extension, the one `ext` supplies.
fn is_frame_id(value: &str) -> bool {
    let Some((kind, id)) = value.split_once('_') else {
        return false;
    };
    !kind.is_empty()
        && kind.len() <= 16
        && kind.chars().all(|c| c.is_ascii_lowercase())
        && !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_extension(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------------------------

fn ack(meta: &FrameMeta, duplicate: bool) -> IngestAck {
    IngestAck {
        v: INGEST_SCHEMA_VERSION,
        session_id: meta.session_id.clone(),
        frame_id: meta.frame_id.clone(),
        sha256: meta.sha256.clone(),
        stored: true,
        duplicate,
    }
}

/// The REL-12 refusal, with the alert on the transition only.
fn refuse_for_space(state: &AppState) -> Option<ApiError> {
    let storage = &state.config.storage;
    let free_gb = vitals::disk_free_gb(&storage.sessions_dir)?;
    if free_gb >= storage.disk_critical_free_gb {
        return None;
    }

    let message = format!(
        "{free_gb:.1} GB free on {} is below the critical threshold of {} GB; this node is \
         refusing new frames until space is freed (REL-12)",
        storage.sessions_dir.display(),
        storage.disk_critical_free_gb
    );
    if !state
        .ingest
        .refusing_for_space
        .swap(true, Ordering::Relaxed)
    {
        tracing::error!(free_gb, "ingest is refusing frames for lack of space");
        state.bus.publish(Alert::critical(
            ErrorCode::DiskFull.as_str(),
            message.clone(),
        ));
    }

    // `DISK_FULL` is not retryable in general (SDD §4.2), but this instance of it is: the very
    // same request succeeds once the operator frees space, and the transfer agent must keep the
    // frame queued rather than abandon it.
    Some(ApiError::new(ErrorCode::DiskFull, message).with_retryable(true))
}

/// Announce, once, that the node is taking frames again.
fn accepting_again(state: &AppState) {
    if state
        .ingest
        .refusing_for_space
        .swap(false, Ordering::Relaxed)
    {
        state.bus.publish(Alert::info(
            ErrorCode::DiskFull.as_str(),
            "ingest is accepting frames again".to_owned(),
        ));
    }
}

/// A frame id already taken by different bytes (SDD §5.11.2, REL-11).
fn conflict(state: &AppState, meta: &FrameMeta, stored_sha256: &str) -> ApiError {
    let message = format!(
        "frame {} of session {} is already stored with checksum {stored_sha256}; the upload \
         claims {}. Raw frames are immutable (REL-11), so nothing was replaced",
        meta.frame_id, meta.session_id, meta.sha256
    );
    // Rare and never benign: two different frames claiming one id means the field node's id
    // counter and its archive have diverged, which no retry can fix and the operator has to know.
    state.bus.publish(Alert::warning(
        ErrorCode::FrameIdConflict.as_str(),
        message.clone(),
    ));

    ApiError::new(ErrorCode::FrameIdConflict, message).with_detail(serde_json::json!({
        "session_id": meta.session_id,
        "frame_id": meta.frame_id,
        "stored_sha256": stored_sha256,
        "offered_sha256": meta.sha256,
    }))
}

/// The bit-flip case: what arrived is not what the metadata describes.
fn checksum_mismatch(state: &AppState, meta: &FrameMeta, got: &str, size: u64) -> ApiError {
    let message = format!(
        "frame {} of session {} arrived corrupted: metadata declares {} ({} bytes), the body \
         hashes to {got} ({size} bytes). Nothing was stored",
        meta.frame_id, meta.session_id, meta.sha256, meta.size
    );
    // Worth an alert rather than only a log: a link that corrupts frames looks, from the field
    // node, exactly like a link that works — the frames leave and the queue drains.
    state.bus.publish(Alert::warning(
        ErrorCode::ChecksumMismatch.as_str(),
        message.clone(),
    ));

    ApiError::new(ErrorCode::ChecksumMismatch, message).with_detail(serde_json::json!({
        "session_id": meta.session_id,
        "frame_id": meta.frame_id,
        "declared_sha256": meta.sha256,
        "received_sha256": got,
        "declared_size": meta.size,
        "received_size": size,
    }))
}

/// A failure of this node's own storage or index, as the sender sees it.
///
/// `INTERNAL` (500) and retryable: nothing about the request is wrong, so marking the frame
/// `failed` would abandon a good frame over a full inode table or an unmounted volume. A 5xx
/// keeps it queued (SDD §5.10.1) until the operator fixes the node.
impl From<MirrorError> for ApiError {
    fn from(error: MirrorError) -> Self {
        tracing::error!(%error, "the session archive could not accept a frame");
        Self::new(ErrorCode::Internal, error.to_string()).with_retryable(true)
    }
}

impl From<MirrorError> for ApiFailure {
    fn from(error: MirrorError) -> Self {
        Self(error.into())
    }
}

impl From<JournalError> for ApiFailure {
    fn from(error: JournalError) -> Self {
        Self(MirrorError::from(error).into())
    }
}

/// Map a multipart failure onto the closed code enum of SDD §4.2.
///
/// The split follows the status axum's own error already carries, because that status is exactly
/// the "is this the sender's fault" question: a malformed or oversized body is definitive
/// (`VALIDATION`, 422), while a body that stopped arriving is a transport failure that must come
/// back as a 5xx so the transfer agent requeues instead of giving up (SDD §5.10.1).
fn from_multipart(error: &axum::extract::multipart::MultipartError) -> ApiError {
    if error.status().is_server_error() {
        ApiError::new(
            ErrorCode::Internal,
            format!("the upload did not finish arriving: {}", error.body_text()),
        )
        .with_retryable(true)
    } else {
        ApiError::new(
            ErrorCode::Validation,
            format!("malformed ingest body: {}", error.body_text()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_for, multipart, sha256_hex, TestApp, TestNode};
    use astroctl_core::bus::{EventSubscriber, Recv};
    use astroctl_core::event::Topic;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use serde_json::{json, Value};
    use std::time::Duration;
    use tower::ServiceExt as _;

    const TOKEN: &str = "s3cret";

    // --- unit: the metadata contract ----------------------------------------------------------

    fn meta() -> FrameMeta {
        FrameMeta {
            v: 1,
            session_id: "2026-07-29_ngc7000".to_owned(),
            frame_id: "light_00042".to_owned(),
            sha256: "a".repeat(64),
            size: 25 * 1024 * 1024,
            ext: "cr3".to_owned(),
            capture: None,
            session: None,
        }
    }

    #[test]
    fn the_reference_metadata_validates() {
        assert!(validate(&meta()).is_ok());
    }

    #[test]
    fn a_version_this_node_does_not_speak_is_refused_by_name() {
        let error = validate(&FrameMeta { v: 2, ..meta() }).expect_err("refused");
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains('2'), "{}", error.message);
    }

    /// The check that keeps an upload inside the archive.
    #[test]
    fn no_identifier_can_escape_the_session_directory() {
        for session_id in ["../etc", "a/b", ".hidden", "", &"x".repeat(65)] {
            assert!(
                validate(&FrameMeta {
                    session_id: session_id.to_owned(),
                    ..meta()
                })
                .is_err(),
                "session_id {session_id:?} must be refused"
            );
        }
        for frame_id in [
            "../../light_1",
            "light/00042",
            "light_00042.cr3",
            "nounderscore",
            "_00042",
            "LIGHT_00042",
        ] {
            assert!(
                validate(&FrameMeta {
                    frame_id: frame_id.to_owned(),
                    ..meta()
                })
                .is_err(),
                "frame_id {frame_id:?} must be refused"
            );
        }
        for ext in ["", "cr3/../x", "CR3", "toolongextension"] {
            assert!(
                validate(&FrameMeta {
                    ext: ext.to_owned(),
                    ..meta()
                })
                .is_err(),
                "ext {ext:?} must be refused"
            );
        }
    }

    #[test]
    fn a_checksum_that_is_not_a_sha256_is_refused() {
        for sha in ["", "abc", &"a".repeat(63), &"z".repeat(64)] {
            assert!(
                validate(&FrameMeta {
                    sha256: sha.to_owned(),
                    ..meta()
                })
                .is_err(),
                "sha {sha:?} must be refused"
            );
        }
    }

    #[test]
    fn a_size_outside_the_ceiling_is_refused_before_anything_is_written() {
        assert!(validate(&FrameMeta { size: 0, ..meta() }).is_err());
        assert!(validate(&FrameMeta {
            size: MAX_FRAME_BYTES + 1,
            ..meta()
        })
        .is_err());
        assert!(validate(&FrameMeta {
            size: MAX_FRAME_BYTES,
            ..meta()
        })
        .is_ok());
    }

    /// The frame ids SDD §5.5 actually hands out, and the sidecar name they imply.
    #[test]
    fn the_sdd_5_5_frame_ids_are_accepted() {
        for frame_id in ["light_00042", "dark_00001", "flat_0", "bias_12345678"] {
            assert!(
                validate(&FrameMeta {
                    frame_id: frame_id.to_owned(),
                    ..meta()
                })
                .is_ok(),
                "frame_id {frame_id:?} must be accepted"
            );
        }
    }

    // --- the ingest contract over HTTP: T-ING-1 -----------------------------------------------

    /// The metadata a well-behaved field agent sends for `body`.
    fn wire_meta(session_id: &str, frame_id: &str, body: &[u8]) -> Value {
        json!({
            "v": INGEST_SCHEMA_VERSION,
            "session_id": session_id,
            "frame_id": frame_id,
            "sha256": sha256_hex(body),
            "size": body.len(),
            "ext": "cr3",
        })
    }

    async fn post_parts(app: &TestApp, parts: &[(&str, &[u8])]) -> (StatusCode, Value) {
        let (content_type, body) = multipart(parts);
        let request = Request::builder()
            .method("POST")
            .uri("/api/ingest")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .expect("request builds");

        let response = app
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// One upload, exactly as SDD §5.11.1 describes the body.
    async fn upload(
        app: &TestApp,
        session_id: &str,
        frame_id: &str,
        body: &[u8],
    ) -> (StatusCode, Value) {
        let meta = wire_meta(session_id, frame_id, body).to_string();
        post_parts(app, &[(META_PART, meta.as_bytes()), (FRAME_PART, body)]).await
    }

    // --- the preview seam (M1-T14) ------------------------------------------------------------

    /// A stored frame is offered to the preview pipeline, at the path the worker will be given.
    ///
    /// ADR-13 passes frames by filesystem path, so "the right path" is the whole contract between
    /// this handler and the worker — a path that does not exist produces `NOT_FOUND` from Python
    /// and a preview that silently never appears.
    #[tokio::test]
    async fn a_stored_frame_is_offered_for_preview_at_the_path_the_worker_will_read() {
        let node = TestNode::authenticated(TOKEN);
        let app = app_for(&node).await;

        let (status, _) = upload(&app, "2026-07-29_m31", "light_00001", b"raw bytes").await;
        assert_eq!(status, StatusCode::OK);

        let job = app
            .state
            .preview_queue
            .take_for_test()
            .expect("a stored frame is queued for preview");
        assert!(
            job.ends_with("2026-07-29_m31/frames/light_00001.cr3"),
            "the worker is handed the archived frame, extension and all: {}",
            job.display()
        );
        assert!(
            tokio::fs::try_exists(&job).await.unwrap_or(false),
            "the offered path must exist by the time it is offered — the journal row is written \
             first, and the frame before that"
        );
    }

    /// A re-upload of a frame this node already holds must not re-queue it. The dedup fast path
    /// never touches the file, so previewing again would burn a worker slot to produce the
    /// identical JPEG — and under a field node retrying a backlog, repeatedly.
    #[tokio::test]
    async fn a_duplicate_upload_does_not_queue_a_second_preview() {
        let node = TestNode::authenticated(TOKEN);
        let app = app_for(&node).await;

        upload(&app, "2026-07-29_m31", "light_00001", b"raw bytes").await;
        assert!(app.state.preview_queue.take_for_test().is_some());

        let (status, body) = upload(&app, "2026-07-29_m31", "light_00001", b"raw bytes").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["duplicate"], true);
        assert!(
            app.state.preview_queue.take_for_test().is_none(),
            "a duplicate is acked, not re-previewed"
        );
    }

    /// A refused upload leaves nothing behind for the worker. The 507 short-circuits before a byte
    /// of the frame is read, so there is no file to preview and queueing one would hand the worker
    /// a path to nothing.
    #[tokio::test]
    async fn a_frame_refused_for_space_is_never_queued_for_preview() {
        let node = TestNode::authenticated(TOKEN).with_disk_thresholds(100_000.0, 99_999.0);
        let app = app_for(&node).await;

        let (status, _) = upload(&app, "2026-07-29_m31", "light_00001", b"raw bytes").await;
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
        assert!(app.state.preview_queue.take_for_test().is_none());
    }

    /// `/api/stacking/stats` reports the node's real last-preview time — the field node
    /// republishes this into `stack.status` (§4.3, USB-06), so a hardcoded `null` here would make
    /// the operator's panel say "no preview yet" all night.
    #[tokio::test]
    async fn stats_reports_the_last_preview_the_node_actually_produced() {
        let node = TestNode::authenticated(TOKEN);
        let app = app_for(&node).await;

        upload(&app, "2026-07-29_m31", "light_00001", b"raw bytes").await;
        let before = get(&app, "/api/stacking/stats").await;
        assert_eq!(
            before["last_preview_ts"],
            Value::Null,
            "ingest alone produces no preview — the worker does"
        );

        app.state.previews.publish(b"\xff\xd8jpeg", "light_00001");

        let after = get(&app, "/api/stacking/stats").await;
        assert!(
            after["last_preview_ts"].is_string(),
            "the timestamp must follow the preview: {after}"
        );
        assert_eq!(after["frame_count"], 1);
    }

    async fn get(app: &TestApp, path: &str) -> Value {
        let request = Request::builder()
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .expect("request builds");
        let response = app
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    /// A raw request returning status + headers + body, for the pre-flight's header assertion.
    async fn request_raw(
        app: &TestApp,
        method: &str,
        path: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .expect("request builds");
        let response = app
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        (status, headers, bytes.to_vec())
    }

    /// Alert codes seen on the bus, so a test can assert "once, not per attempt".
    async fn alerts(sub: &mut EventSubscriber) -> Vec<String> {
        let mut codes = Vec::new();
        while let Ok(Recv::Event(event)) =
            tokio::time::timeout(Duration::from_millis(50), sub.recv()).await
        {
            if event.topic == Topic::Alert {
                codes.push(event.data["code"].as_str().unwrap_or_default().to_owned());
            }
        }
        codes
    }

    fn stored_frame_path(app: &TestApp, session_id: &str, frame_id: &str) -> std::path::PathBuf {
        app.state
            .ingest
            .archive()
            .root()
            .join(session_id)
            .join(crate::mirror::FRAMES_DIR)
            .join(format!("{frame_id}.cr3"))
    }

    #[tokio::test]
    async fn a_frame_is_stored_and_acked_with_the_checksum_the_sender_verifies() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"a raw frame, as far as this test is concerned".as_slice();

        let (status, ack) = upload(&app, "2026-07-29_ngc7000", "light_00042", body).await;

        assert_eq!(status, StatusCode::OK, "{ack}");
        assert_eq!(ack["v"], 1);
        assert_eq!(ack["session_id"], "2026-07-29_ngc7000");
        assert_eq!(ack["frame_id"], "light_00042");
        assert_eq!(ack["sha256"], sha256_hex(body));
        assert_eq!(ack["stored"], true);
        assert_eq!(ack["duplicate"], false);

        let stored = stored_frame_path(&app, "2026-07-29_ngc7000", "light_00042");
        assert_eq!(tokio::fs::read(&stored).await.unwrap(), body);

        let row = app
            .state
            .ingest
            .archive()
            .lookup("2026-07-29_ngc7000", "light_00042")
            .await
            .unwrap()
            .expect("the journal recorded it");
        assert_eq!(row.sha256, sha256_hex(body));
        assert_eq!(row.rel_path, "2026-07-29_ngc7000/frames/light_00042.cr3");
    }

    /// T-ING-1, first clause: a bit-flipped upload is refused, nothing is stored, tmp is cleaned.
    #[tokio::test]
    async fn a_bit_flipped_upload_is_refused_and_leaves_nothing_behind() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let mut sub = app.state.bus.subscribe();
        let honest = b"the bytes the field node hashed".as_slice();
        let mut corrupted = honest.to_vec();
        corrupted[3] ^= 0x01;

        // The metadata describes the good bytes; the body carries the flipped ones — which is
        // exactly what a link that corrupts in flight produces.
        let meta = wire_meta("S", "light_00001", honest).to_string();
        let (status, body) = post_parts(
            &app,
            &[(META_PART, meta.as_bytes()), (FRAME_PART, &corrupted)],
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "CHECKSUM_MISMATCH");
        assert_eq!(body["retryable"], false, "the same bytes will fail again");
        assert_eq!(body["detail"]["declared_sha256"], sha256_hex(honest));
        assert_eq!(body["detail"]["received_sha256"], sha256_hex(&corrupted));

        assert!(
            app.state
                .ingest
                .archive()
                .lookup("S", "light_00001")
                .await
                .unwrap()
                .is_none(),
            "nothing may be recorded"
        );
        let frames = app
            .state
            .ingest
            .archive()
            .root()
            .join("S")
            .join(crate::mirror::FRAMES_DIR);
        let mut entries = tokio::fs::read_dir(&frames).await.unwrap();
        assert!(
            entries.next_entry().await.unwrap().is_none(),
            "neither the frame nor its temporary may survive"
        );
        assert_eq!(alerts(&mut sub).await, vec!["CHECKSUM_MISMATCH"]);
    }

    /// T-ING-1, second clause, and the reason SDD §5.10.3's blind retry is safe.
    #[tokio::test]
    async fn a_duplicate_upload_is_acked_without_rewriting_the_frame() {
        use std::os::unix::fs::MetadataExt as _;

        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"one frame".as_slice();

        let (first, _) = upload(&app, "S", "light_00001", body).await;
        assert_eq!(first, StatusCode::OK);
        let stored = stored_frame_path(&app, "S", "light_00001");
        let before = tokio::fs::metadata(&stored).await.unwrap();

        let (status, ack) = upload(&app, "S", "light_00001", body).await;

        assert_eq!(status, StatusCode::OK, "{ack}");
        assert_eq!(ack["stored"], true);
        assert_eq!(ack["duplicate"], true);
        // A rewrite would arrive by rename and so would change the inode. Comparing inodes is the
        // difference between "the content is still right" and "the file was never touched".
        let after = tokio::fs::metadata(&stored).await.unwrap();
        assert_eq!(before.ino(), after.ino(), "the stored frame was replaced");
        assert_eq!(get(&app, "/api/stacking/stats").await["frame_count"], 1);
    }

    /// T-ING-1, third clause: same id, different bytes. REL-11 forbids the overwrite.
    #[tokio::test]
    async fn the_same_frame_id_with_different_bytes_is_a_conflict_and_the_original_stands() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let mut sub = app.state.bus.subscribe();
        upload(&app, "S", "light_00001", b"the original").await;
        let _ = alerts(&mut sub).await;

        let (status, body) = upload(&app, "S", "light_00001", b"something else").await;

        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "FRAME_ID_CONFLICT");
        assert_eq!(body["detail"]["stored_sha256"], sha256_hex(b"the original"));
        assert_eq!(
            tokio::fs::read(stored_frame_path(&app, "S", "light_00001"))
                .await
                .unwrap(),
            b"the original"
        );
        assert_eq!(alerts(&mut sub).await, vec!["FRAME_ID_CONFLICT"]);
    }

    /// T-ING-1, fourth clause, plus SDD §5.10.4's one-alert rule.
    #[tokio::test]
    async fn below_the_critical_threshold_ingest_refuses_with_507_and_alerts_once() {
        // Thresholds above any plausible free space, so every attempt is a refusal — the same
        // technique the disk watchdog's tests use, but through the validator, which caps a
        // threshold at 100 000 GB.
        let node = TestNode::authenticated(TOKEN).with_disk_thresholds(100_000.0, 99_999.0);
        let app = app_for(&node).await;
        let mut sub = app.state.bus.subscribe();

        let (status, body) = upload(&app, "S", "light_00001", b"a frame").await;

        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
        assert_eq!(body["code"], "DISK_FULL");
        // Overridden from the code's default: freeing space makes this identical request succeed,
        // and the transfer agent must keep the frame queued rather than fail it.
        assert_eq!(body["retryable"], true);
        assert_eq!(alerts(&mut sub).await, vec!["DISK_FULL"]);

        let (status, _) = upload(&app, "S", "light_00002", b"another frame").await;
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
        assert!(
            alerts(&mut sub).await.is_empty(),
            "a full disk is one alert, not one per refused frame"
        );

        assert!(
            !tokio::fs::try_exists(app.state.ingest.archive().root().join("S"))
                .await
                .unwrap_or(false),
            "a refused ingest must not even create the session directory"
        );
    }

    /// IPP-15: a session that finished hours ago still takes frames, and the manifest says so
    /// rather than treating the late arrival as an error.
    #[tokio::test]
    async fn frames_arriving_long_after_a_session_ended_are_still_stored() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let ended = astroctl_core::event::now_millis() - chrono::Duration::hours(4);

        let body = b"the first frame".as_slice();
        let mut meta = wire_meta("S", "light_00001", body);
        meta["session"] = json!({
            "target": {"name": "NGC 7000"},
            "created_ts": ended.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        });
        let meta = meta.to_string();
        let (status, _) =
            post_parts(&app, &[(META_PART, meta.as_bytes()), (FRAME_PART, body)]).await;
        assert_eq!(status, StatusCode::OK);

        // …and a frame that turns up later with no session block at all.
        let (status, ack) = upload(&app, "S", "light_00099", b"a straggler").await;
        assert_eq!(status, StatusCode::OK, "{ack}");

        let stats = get(&app, "/api/stacking/stats").await;
        assert_eq!(stats["session_id"], "S");
        assert_eq!(stats["frame_count"], 2);
        assert!(stats["last_ingest_ts"].is_string(), "{stats}");
        assert_eq!(stats["last_preview_ts"], Value::Null);

        let manifest: crate::mirror::Manifest = serde_json::from_slice(
            &tokio::fs::read(
                app.state
                    .ingest
                    .archive()
                    .root()
                    .join("S")
                    .join(crate::mirror::SESSION_JSON),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.mirror.frame_count, 2);
        assert_eq!(
            manifest.created_ts.timestamp_millis(),
            ended.timestamp_millis(),
            "the session's creation time is the field node's, not the mirror's"
        );
        assert_eq!(manifest.target, Some(json!({"name": "NGC 7000"})));
    }

    /// The regression that would otherwise be found by the first real CR3: axum's default body
    /// limit is 2 MiB, and a frame is an order of magnitude past it. 3 MiB is enough to prove the
    /// limit was raised without spending a real frame's worth of memory in a unit test.
    #[tokio::test]
    async fn a_frame_larger_than_the_default_body_limit_is_accepted() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = vec![0xa5_u8; 3 * 1024 * 1024];

        let (status, ack) = upload(&app, "S", "light_00001", &body).await;

        assert_eq!(status, StatusCode::OK, "{ack}");
        assert_eq!(ack["sha256"], sha256_hex(&body));
        assert_eq!(
            tokio::fs::metadata(stored_frame_path(&app, "S", "light_00001"))
                .await
                .unwrap()
                .len(),
            body.len() as u64
        );
    }

    #[tokio::test]
    async fn stats_answers_before_anything_has_been_ingested() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let stats = get(&app, "/api/stacking/stats").await;
        assert_eq!(stats["v"], 1);
        assert_eq!(stats["session_id"], Value::Null);
        assert_eq!(stats["frame_count"], 0);
        assert_eq!(stats["last_ingest_ts"], Value::Null);
    }

    /// Streaming to disk requires the destination before the bytes, so the order is a contract.
    #[tokio::test]
    async fn the_metadata_part_must_arrive_first() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"a frame".as_slice();
        let meta = wire_meta("S", "light_00001", body).to_string();

        let (status, answer) =
            post_parts(&app, &[(FRAME_PART, body), (META_PART, meta.as_bytes())]).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["code"], "VALIDATION");

        let (status, answer) = post_parts(&app, &[(META_PART, meta.as_bytes())]).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert!(
            answer["message"]
                .as_str()
                .unwrap_or_default()
                .contains(FRAME_PART),
            "{answer}"
        );
    }

    #[tokio::test]
    async fn a_body_longer_than_its_metadata_declares_is_refused_mid_stream() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"ten bytes!".as_slice();
        let mut meta = wire_meta("S", "light_00001", body);
        meta["size"] = json!(body.len() - 1);
        let meta = meta.to_string();

        let (status, answer) =
            post_parts(&app, &[(META_PART, meta.as_bytes()), (FRAME_PART, body)]).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["code"], "CHECKSUM_MISMATCH");
        assert!(app
            .state
            .ingest
            .archive()
            .lookup("S", "light_00001")
            .await
            .unwrap()
            .is_none());
    }

    /// The path-traversal case at the HTTP boundary, not just in `validate`.
    #[tokio::test]
    async fn metadata_that_would_write_outside_the_archive_is_refused() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"a frame".as_slice();
        let mut meta = wire_meta("../../etc", "light_00001", body);
        meta["session_id"] = json!("../../etc");
        let meta = meta.to_string();

        let (status, answer) =
            post_parts(&app, &[(META_PART, meta.as_bytes()), (FRAME_PART, body)]).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["code"], "VALIDATION");
    }

    /// An unknown field is a version skew or a typo, and losing capture metadata silently is the
    /// failure `deny_unknown_fields` exists to prevent.
    #[tokio::test]
    async fn metadata_this_node_does_not_understand_is_refused_rather_than_partly_ignored() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"a frame".as_slice();
        let mut meta = wire_meta("S", "light_00001", body);
        meta["exposure_s"] = json!(120);
        let meta = meta.to_string();

        let (status, answer) =
            post_parts(&app, &[(META_PART, meta.as_bytes()), (FRAME_PART, body)]).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["code"], "VALIDATION");
    }

    /// The journal's `source` column, end to end: with connection info present it records the
    /// peer, which behind the ADR-07 proxy is the field node.
    #[tokio::test]
    async fn the_journal_records_who_uploaded_a_frame() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"a frame".as_slice();
        let meta = wire_meta("S", "light_00001", body).to_string();
        let (content_type, wire) = multipart(&[(META_PART, meta.as_bytes()), (FRAME_PART, body)]);

        let mut request = Request::builder()
            .method("POST")
            .uri("/api/ingest")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(wire))
            .expect("request builds");
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([10, 8, 0, 2], 51_820))));

        let response = app
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);

        let row = app
            .state
            .ingest
            .archive()
            .lookup("S", "light_00001")
            .await
            .unwrap()
            .expect("recorded");
        assert_eq!(row.source.as_deref(), Some("10.8.0.2:51820"));
    }
    /// The §5.11.1 pre-flight, landed after M1-T11 measured what its absence costs: a duplicate
    /// discovered only after ~200 s of shaped-link body. 204 + the stored hash for a held frame;
    /// a NOT_FOUND envelope otherwise; HEAD answered from the same route with no body. The
    /// sender treats any non-204 as "upload", so the 404's shape only needs to not be a 204.
    #[tokio::test]
    async fn the_preflight_reports_a_stored_frame_and_404s_an_unknown_one() {
        let app = app_for(&TestNode::authenticated(TOKEN)).await;
        let body = b"preflight-frame-bytes".to_vec();
        let (status, ack) = upload(&app, "2026-01-01_m42", "light_00001", &body).await;
        assert_eq!(status, StatusCode::OK);
        let stored_sha = ack["sha256"].as_str().expect("ack carries sha").to_owned();

        let (status, headers, _) =
            request_raw(&app, "GET", "/api/ingest/2026-01-01_m42/light_00001").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            headers.get("x-astroctl-sha256").unwrap().to_str().unwrap(),
            stored_sha
        );

        let (status, _, body) =
            request_raw(&app, "GET", "/api/ingest/2026-01-01_m42/light_09999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let json: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(json["code"], "NOT_FOUND");

        let (status, headers, body) =
            request_raw(&app, "HEAD", "/api/ingest/2026-01-01_m42/light_00001").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(headers.contains_key("x-astroctl-sha256"));
        assert!(body.is_empty(), "a HEAD answer carries no body");
    }
}
