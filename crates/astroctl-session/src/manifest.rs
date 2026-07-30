//! What the session tree persists: `session.json` and `control/quality_<id>.json`.
//!
//! Both are v1 schemas per SDD §6, both are written with the tmp-fsync-rename discipline of
//! [`crate::durable`], and both are meant to be readable by a human with `cat` when something has
//! gone wrong at 2 a.m. — which is why they stay JSON on disk rather than joining the one SQLite
//! database the field node is allowed (SDD §6: "everything else stays human-readable and travels
//! with the data").

use astroctl_core::config::EquipmentConfig;
use astroctl_core::event::ts_rfc3339_millis;
use astroctl_core::types::CameraSettings;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::frame::FrameId;

/// Schema version of `session.json` and the quality sidecars (SDD §2, §6).
pub const SCHEMA_VERSION: u16 = 1;

/// The equipment profile stamped onto a session, snapshotted from configuration at creation.
///
/// A *snapshot*, not a reference: the operator may edit `field-node.yaml` between sessions, and
/// the calibration library matches darks and flats to the equipment a frame was actually taken
/// with (PRD §8.1 `equipment`). A session that reported today's configuration for last week's
/// frames would silently match the wrong calibration masters.
///
/// Structurally identical to [`EquipmentConfig`] and deliberately a separate type: that one is a
/// config schema (`deny_unknown_fields`, deserialize-only, validated against the operator's YAML),
/// this one is a persisted record. Deriving `Serialize` on the config type would make every future
/// config field an automatic addition to this file's schema.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Equipment {
    /// Telescope description, e.g. `"SW 200PDS f/5"`.
    pub telescope: String,
    /// Camera description, e.g. `"Canon R10"`.
    pub camera: String,
    /// Filter description, or `"none"`.
    pub filter: String,
}

impl From<&EquipmentConfig> for Equipment {
    fn from(config: &EquipmentConfig) -> Self {
        Self {
            telescope: config.telescope.clone(),
            camera: config.camera.clone(),
            filter: config.filter.clone(),
        }
    }
}

/// `sessions/<session_id>/session.json` (SDD §5.5, §6).
///
/// Unknown fields are **ignored rather than rejected**, unlike the operator-facing configuration.
/// This file is written by the program and read back by it, possibly after a downgrade: a v2 node
/// that adds a field and is then rolled back must leave a v1 node able to read the counter REL-04
/// depends on. Refusing to parse would turn a rollback into an ID-reuse incident.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SessionManifest {
    /// Schema version (SDD §2).
    pub v: u16,
    /// `YYYY-MM-DD_<slug>` — also the directory name.
    pub session_id: String,
    /// When the session directory was created.
    #[serde(with = "ts_rfc3339_millis")]
    pub created_ts: DateTime<Utc>,
    /// What the session is pointed at, or `null` when nothing has said.
    ///
    /// Opaque JSON on purpose. The target model belongs to the Phase 2a sequence work (SDD §5.6)
    /// and does not exist yet; reserving the field now means its arrival is not a schema change,
    /// and the frame store does not invent a domain model it does not own. `astroctl-stack` reads
    /// the same field the same way (§5.11.3).
    pub target: Option<serde_json::Value>,
    /// The equipment profile in force when the session was created.
    pub equipment: Equipment,
    /// The highest frame sequence number ever *granted* in this session (REL-04).
    ///
    /// The load-bearing field of this file. It is persisted before `reserve_frame_id` returns, so
    /// a crash between the grant and the frame burns one id — the cheap failure — while reuse,
    /// which would overwrite a captured frame, cannot happen. It is deliberately **not** a count of
    /// frames on disk: those two numbers diverge legitimately (a burned id, an aborted capture),
    /// and a reader that needs the count reads the directory, which cannot go stale.
    pub frames_reserved: u64,
    /// Reserved for the Phase 2a sequence FSM (SDD §5.6: "the persistence format is already
    /// reserved"). Always present, always `null` in Phase 1.
    pub sequence_state: Option<serde_json::Value>,
}

impl SessionManifest {
    /// A fresh session's manifest.
    pub(crate) fn new(
        session_id: String,
        created_ts: DateTime<Utc>,
        target: Option<serde_json::Value>,
        equipment: Equipment,
    ) -> Self {
        Self {
            v: SCHEMA_VERSION,
            session_id,
            created_ts,
            target,
            equipment,
            frames_reserved: 0,
            sequence_state: None,
        }
    }
}

/// What the capture flow knows about an exposure and the store does not.
///
/// [`CameraSettings`] is reused rather than restated: it is already the shape of
/// `/api/camera/settings` (SDD §5.8.1) and of what the driver reports, so the sidecar records the
/// tokens the camera actually used instead of a lossy re-rendering of them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CaptureParams {
    /// When the exposure started, as the capture flow observed it — not when the file landed.
    #[serde(with = "ts_rfc3339_millis")]
    pub started_ts: DateTime<Utc>,
    /// Exposure length in seconds.
    ///
    /// Separate from `settings.shutter` because a bulb exposure's shutter token is the string
    /// `"bulb"` and its length is the `bulb_seconds` of the capture request (CAM-04). A consumer
    /// asking "how long was this frame" must not have to parse camera tokens to find out.
    pub exposure_s: f64,
    /// The camera's own settings tokens at the moment of capture.
    pub settings: CameraSettings,
}

/// `sessions/<session_id>/control/quality_<id>.json` (SDD §5.5, §6: ts, exposure, iso, format,
/// sha256, size).
///
/// Phase 1 records what the capture flow knows. The measured fields — FWHM, HFR, star count, the
/// quality score of IPP-03 — join it when the control pipeline lands; `deny_unknown_fields` is
/// therefore absent here for the same reason as on the manifest.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Quality {
    /// Schema version (SDD §2).
    pub v: u16,
    /// The frame this describes.
    pub frame_id: FrameId,
    /// When the exposure started.
    #[serde(with = "ts_rfc3339_millis")]
    pub started_ts: DateTime<Utc>,
    /// Exposure length in seconds.
    pub exposure_s: f64,
    /// The camera settings the frame was taken with.
    pub settings: CameraSettings,
    /// Lowercase hex SHA-256 of the stored frame, as read back from disk after the commit.
    pub sha256: String,
    /// Size of the stored frame.
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use astroctl_core::event::now_millis;
    use astroctl_core::types::ImageFormat;

    fn settings() -> CameraSettings {
        CameraSettings {
            iso: "1600".to_owned(),
            shutter: "bulb".to_owned(),
            aperture: None,
            format: ImageFormat::Raw,
        }
    }

    #[test]
    fn a_manifest_round_trips_through_the_json_it_is_stored_as() {
        let manifest = SessionManifest::new(
            "2026-07-30_ngc7000".to_owned(),
            now_millis(),
            Some(serde_json::json!({"name": "NGC 7000"})),
            Equipment {
                telescope: "SW 200PDS f/5".to_owned(),
                camera: "Canon R10".to_owned(),
                filter: "none".to_owned(),
            },
        );

        let json = serde_json::to_vec_pretty(&manifest).unwrap();
        assert_eq!(
            serde_json::from_slice::<SessionManifest>(&json).unwrap(),
            manifest
        );
    }

    /// A downgrade must still be able to read the counter REL-04 depends on.
    #[test]
    fn a_manifest_from_a_newer_writer_still_parses() {
        let json = serde_json::json!({
            "v": 2,
            "session_id": "2026-07-30_ngc7000",
            "created_ts": "2026-07-30T21:04:05.123Z",
            "target": null,
            "equipment": {"telescope": "t", "camera": "c", "filter": "none"},
            "frames_reserved": 42,
            "sequence_state": null,
            "dither_pattern": "spiral"
        });

        let manifest: SessionManifest =
            serde_json::from_value(json).expect("v2 fields are ignored");

        assert_eq!(manifest.frames_reserved, 42);
    }

    #[test]
    fn a_sidecar_round_trips_and_names_its_frame_as_a_string() {
        let quality = Quality {
            v: SCHEMA_VERSION,
            frame_id: crate::frame::FrameId::new(crate::frame::FrameKind::Light, 42),
            started_ts: now_millis(),
            exposure_s: 120.0,
            settings: settings(),
            sha256: "abcd".to_owned(),
            size_bytes: 25_600_000,
        };

        let json = serde_json::to_value(&quality).unwrap();

        assert_eq!(json["frame_id"], "light_00042");
        assert_eq!(json["settings"]["format"], "RAW");
        assert_eq!(serde_json::from_value::<Quality>(json).unwrap(), quality);
    }

    #[test]
    fn the_equipment_snapshot_copies_the_configured_profile() {
        let config: EquipmentConfig = serde_json::from_value(serde_json::json!({
            "telescope": "SW 200PDS f/5",
            "camera": "Canon R10",
            "filter": "none"
        }))
        .unwrap();

        let equipment = Equipment::from(&config);

        assert_eq!(equipment.telescope, "SW 200PDS f/5");
        assert_eq!(equipment.camera, "Canon R10");
        assert_eq!(equipment.filter, "none");
    }
}
