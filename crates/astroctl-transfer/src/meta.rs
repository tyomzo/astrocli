//! Turning a queue row into the `meta` part — SDD §5.5's layout, §5.11.1's schema.
//!
//! The transfer agent is handed a frame path and has to produce a `session_id`, an `ext`, and the
//! two opaque blobs the receiver mirrors. All four come out of the session layout of §5.5, which
//! is fixed by a fixture (`astroctl-session/testdata/session-layout.txt`) that two crates already
//! assert against:
//!
//! ```text
//! sessions/<session_id>/session.json
//! sessions/<session_id>/frames/light_00042.cr3
//! sessions/<session_id>/control/quality_00042.json
//! ```
//!
//! # Why the session id is derived from the path rather than held
//!
//! The agent could be told the current session at startup, and that would be wrong: a node that
//! runs past midnight, or that is asked to start a new session, would keep filing new frames under
//! the old id and the archive would mirror them into the wrong directory. The path of the frame
//! that was actually saved is the only source that cannot go stale, because it is the directory
//! the bytes are in.
//!
//! # Why `capture` and `session` are read as opaque JSON
//!
//! §5.11.1 makes both deliberately opaque: "the field node owns those schemas (§5.5), and a second
//! declaration here would drift from it". Re-declaring them in *this* crate to reserialize them
//! would recreate exactly the drift the receiver avoided — a field added to the sidecar by the
//! capture flow would be silently dropped in transit, with no symptom. So the sidecar is forwarded
//! byte-equivalently, and the only structure this module asserts is the three keys §5.11.1's
//! `session` object is allowed to contain.

use std::path::{Path, PathBuf};

use crate::journal::Entry;
use crate::upload::FrameUpload;

/// The directory holding the frames, per §5.5.
const FRAMES_DIR: &str = "frames";
/// The directory holding the sidecars, per §5.5.
const CONTROL_DIR: &str = "control";
/// The session manifest, per §5.5.
const SESSION_JSON: &str = "session.json";

/// The three keys §5.11.1's `session` object may carry. The receiver is `deny_unknown_fields`, so
/// forwarding `session.json` wholesale would be a `422 VALIDATION` on every frame — it also holds
/// `v`, `session_id`, `frames_reserved` and `sequence_state`.
const SESSION_KEYS: [&str; 3] = ["target", "equipment", "created_ts"];

/// The session directory a frame lives in — `<sessions>/<session_id>`.
///
/// Returns `None` when the path is not shaped like §5.5's layout, which is a frame this node did
/// not write and cannot describe.
#[must_use]
pub fn session_dir(frame_path: &Path) -> Option<&Path> {
    let frames = frame_path.parent()?;
    if frames.file_name()? != FRAMES_DIR {
        return None;
    }
    frames.parent()
}

/// The session id a frame belongs to, from its path.
#[must_use]
pub fn session_id(frame_path: &Path) -> Option<String> {
    Some(
        session_dir(frame_path)?
            .file_name()?
            .to_string_lossy()
            .into_owned(),
    )
}

/// The sidecar for a frame id — `control/quality_<id>.json`, where `<id>` is the frame id with its
/// kind prefix removed.
///
/// §5.5 spells the frame `light_<id>.cr3` and its sidecar `quality_<id>.json`, so the sidecar drops
/// the kind. The layout fixture's comment says exactly this, and getting it wrong costs nothing
/// visible — the upload simply carries no capture metadata — which is why it is worth a test.
#[must_use]
pub fn quality_path(session_dir: &Path, frame_id: &str) -> PathBuf {
    let numeric = frame_id.split_once('_').map_or(frame_id, |(_, id)| id);
    session_dir
        .join(CONTROL_DIR)
        .join(format!("quality_{numeric}.json"))
}

/// The extension the archive will store the frame under.
///
/// §5.11.1 requires `[a-z0-9]{1,8}`; anything else is rejected here rather than by the receiver,
/// because a frame whose name this node cannot express is a frame no retry will fix.
#[must_use]
pub fn extension(frame_path: &Path) -> Option<String> {
    let ext = frame_path.extension()?.to_str()?.to_ascii_lowercase();
    let ok = !ext.is_empty()
        && ext.len() <= 8
        && ext
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    ok.then_some(ext)
}

/// Everything the wire needs, assembled from a queue row and the session directory beside it.
///
/// A missing or unreadable sidecar is **not** a failure. §5.11.2 note 6 makes the derived files no
/// part of the ack on the receiving side, and the same reasoning applies here: refusing to send a
/// durable 25 MB frame because a small JSON file beside it is missing would be trading the thing
/// that matters for the thing that does not. The frame goes without its metadata and says so.
pub async fn frame_upload(entry: &Entry) -> Option<FrameUpload> {
    let ext = extension(&entry.path)?;
    let dir = session_dir(&entry.path);

    let capture = match dir {
        Some(dir) => read_json(&quality_path(dir, &entry.frame_id)).await,
        None => None,
    };
    let session = match dir {
        Some(dir) => read_json(&dir.join(SESSION_JSON))
            .await
            .and_then(session_meta),
        None => None,
    };

    if capture.is_none() {
        tracing::debug!(
            frame = %entry.frame_id,
            "no readable quality sidecar; the frame is uploaded without capture metadata"
        );
    }

    Some(FrameUpload {
        // The row's session id is authoritative — it was derived from the path at enqueue and is
        // what the journal is keyed on, so a mirror written from it cannot disagree with the row
        // that will be marked acked.
        session_id: entry.session_id.clone(),
        frame_id: entry.frame_id.clone(),
        path: entry.path.clone(),
        sha256: entry.sha256.to_ascii_lowercase(),
        size_bytes: entry.size_bytes,
        ext,
        capture,
        session,
    })
}

/// Project `session.json` onto the three keys §5.11.1 allows.
fn session_meta(manifest: serde_json::Value) -> Option<serde_json::Value> {
    let object = manifest.as_object()?;
    let mut projected = serde_json::Map::new();
    for key in SESSION_KEYS {
        if let Some(value) = object.get(key) {
            projected.insert((*key).to_owned(), value.clone());
        }
    }
    (!projected.is_empty()).then_some(serde_json::Value::Object(projected))
}

/// Read a small JSON file, or `None` for any reason at all.
///
/// These are metadata sidecars a few hundred bytes long, so `tokio::fs` is the right hop — this is
/// nothing like the 25 MB frame, which is streamed rather than read.
async fn read_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    /// The layout of SDD §5.5, as `astroctl-session/testdata/session-layout.txt` fixes it. Read
    /// with `include_str!` rather than through a Cargo dependency, exactly as `astroctl-stack`
    /// reads it: it is data, and ADD §5.6's dependency matrix should not acquire an edge to carry
    /// a test fixture.
    const LAYOUT: &str = include_str!("../../astroctl-session/testdata/session-layout.txt");

    fn layout_paths() -> Vec<&'static str> {
        LAYOUT
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    /// The derivations in this module are read off the shared fixture, so a layout change breaks
    /// this test rather than silently mirroring frames into the wrong directory.
    #[test]
    fn the_derivations_agree_with_the_shared_layout_fixture() {
        let paths = layout_paths();
        assert!(
            paths.contains(&"frames/light_00042.cr3"),
            "the fixture moved the frame: {paths:?}"
        );
        assert!(
            paths.contains(&"control/quality_00042.json"),
            "the fixture moved the sidecar: {paths:?}"
        );
        assert!(paths.contains(&SESSION_JSON), "{paths:?}");

        let root = Path::new("/data/astro/sessions/2026-07-29_ngc7000");
        let frame = root.join("frames/light_00042.cr3");

        assert_eq!(session_dir(&frame), Some(root));
        assert_eq!(session_id(&frame).as_deref(), Some("2026-07-29_ngc7000"));
        assert_eq!(extension(&frame).as_deref(), Some("cr3"));
        assert_eq!(
            quality_path(root, "light_00042"),
            root.join("control/quality_00042.json"),
            "the sidecar drops the frame id's kind prefix"
        );
    }

    #[test]
    fn a_path_that_is_not_the_session_layout_yields_nothing() {
        // No `frames/` component: not a frame this node wrote.
        assert_eq!(session_dir(Path::new("/tmp/light_00042.cr3")), None);
        assert_eq!(session_id(Path::new("/tmp/light_00042.cr3")), None);
        assert_eq!(
            session_dir(Path::new("/data/s1/preview/light_00042.jpg")),
            None
        );
    }

    /// §5.11.1 requires `[a-z0-9]{1,8}`. Rejecting here rather than at the far end turns a 25 MB
    /// round trip into a local decision.
    #[test]
    fn only_an_extension_the_archive_can_store_is_accepted() {
        for (name, expected) in [
            ("f.cr3", Some("cr3")),
            ("f.CR3", Some("cr3")),
            ("f.fits", Some("fits")),
            ("f.12345678", Some("12345678")),
            ("f.123456789", None),
            ("f.tar-gz", None),
            ("f", None),
            ("f.", None),
        ] {
            assert_eq!(
                extension(Path::new(name)).as_deref(),
                expected,
                "for {name}"
            );
        }
    }

    #[tokio::test]
    async fn the_sidecar_and_the_manifest_projection_ride_along() {
        let dir = TempDir::new();
        let session = dir.path().join("2026-07-29_ngc7000");
        tokio::fs::create_dir_all(session.join("frames"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(session.join("control"))
            .await
            .unwrap();
        let frame = session.join("frames/light_00042.cr3");
        tokio::fs::write(&frame, b"raw").await.unwrap();
        tokio::fs::write(
            session.join("control/quality_00042.json"),
            br#"{"v":1,"frame_id":"light_00042","exposure_s":120.0,"sha256":"aa","size_bytes":3}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            session.join("session.json"),
            br#"{"v":1,"session_id":"2026-07-29_ngc7000","created_ts":"2026-07-29T18:00:00.000Z",
                 "target":{"name":"NGC 7000"},
                 "equipment":{"telescope":"SW 200PDS","camera":"R10","filter":"none"},
                 "frames_reserved":42,"sequence_state":null}"#,
        )
        .await
        .unwrap();

        let entry = crate::journal::Entry {
            session_id: "2026-07-29_ngc7000".to_owned(),
            frame_id: "light_00042".to_owned(),
            path: frame,
            sha256: "AA".repeat(32),
            size_bytes: 3,
            state: crate::journal::State::Queued,
            attempts: 0,
            queued_ts: astroctl_core::event::now_millis(),
            acked_ts: None,
            reclaimable: false,
            last_error: None,
        };

        let upload = frame_upload(&entry).await.expect("the layout is intact");
        assert_eq!(upload.ext, "cr3");
        assert_eq!(
            upload.sha256,
            "aa".repeat(32),
            "lowercased for the echo comparison"
        );
        // Forwarded verbatim — the capture flow owns this schema (§5.5) and the receiver mirrors
        // it without reading it.
        assert_eq!(upload.capture.as_ref().unwrap()["exposure_s"], 120.0);

        // …and the manifest is projected onto exactly the three keys the receiver accepts, because
        // its `SessionMeta` is `deny_unknown_fields` and `session.json` carries four more.
        let session_meta = upload.session.as_ref().expect("a manifest projection");
        let keys: Vec<&String> = session_meta.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["created_ts", "equipment", "target"]);
        assert_eq!(session_meta["target"]["name"], "NGC 7000");
    }

    /// §5.11.2 note 6's reasoning, applied on this side: a missing sidecar must not stop a durable
    /// frame reaching the archive.
    #[tokio::test]
    async fn a_frame_with_no_sidecar_is_still_uploaded() {
        let dir = TempDir::new();
        let session = dir.path().join("2026-07-29_ngc7000");
        tokio::fs::create_dir_all(session.join("frames"))
            .await
            .unwrap();
        let frame = session.join("frames/light_00001.cr3");
        tokio::fs::write(&frame, b"raw").await.unwrap();

        let entry = crate::journal::Entry {
            session_id: "2026-07-29_ngc7000".to_owned(),
            frame_id: "light_00001".to_owned(),
            path: frame,
            sha256: "aa".repeat(32),
            size_bytes: 3,
            state: crate::journal::State::Queued,
            attempts: 0,
            queued_ts: astroctl_core::event::now_millis(),
            acked_ts: None,
            reclaimable: false,
            last_error: None,
        };

        let upload = frame_upload(&entry)
            .await
            .expect("the frame is still sendable");
        assert!(upload.capture.is_none());
        assert!(upload.session.is_none());
    }
}
