//! Replaying `events.jsonl` — SES-07, in its basic form.
//!
//! The field node writes every published event as one JSON line to `server.log_dir/events.jsonl`
//! (SDD §2). The claim that file makes is that a session can be reconstructed from it. This module
//! is what turns that claim into an assertion: parse the whole file, fold it into a final state,
//! and compare that state to what the API says now.
//!
//! # Fold, don't sample
//!
//! The reconstruction deliberately replays *every* line rather than reading the last one per topic
//! off the end. A log whose middle is corrupt, whose lines are interleaved from two writers, or
//! which contains a truncated final line would pass the cheap check and fail this one — and a
//! truncated final line is exactly what a `kill -9` leaves behind, which is a case this suite goes
//! out of its way to create.

use std::collections::BTreeMap;

use serde_json::Value;

/// A parsed event log.
#[derive(Debug, Default)]
pub struct Replay {
    /// Every line, in file order.
    pub events: Vec<Value>,
    /// Lines that were not parseable JSON, with their 1-based line number.
    ///
    /// Kept rather than rejected: SDD §2's format promises one event per line, and a crash may
    /// leave the last one short. A scenario decides whether a partial tail is acceptable — after a
    /// `kill -9` it is, mid-session it is not.
    pub unparseable: Vec<(usize, String)>,
}

impl Replay {
    /// Parse a whole log.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut replay = Self::default();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(value) => replay.events.push(value),
                Err(_) => replay.unparseable.push((index + 1, line.to_owned())),
            }
        }
        replay
    }

    /// Every event on one topic, in order.
    #[must_use]
    pub fn topic(&self, topic: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| event.get("topic").and_then(Value::as_str) == Some(topic))
            .collect()
    }

    /// The final state, as a topic-to-payload map.
    ///
    /// This is the "reconstruct final state" of SES-07: fold the stream, latest wins per topic.
    /// `alert` and `frame.saved` are excluded because they are not state — they are things that
    /// happened, and "the last alert" is not a state anything can be compared against.
    #[must_use]
    pub fn final_state(&self) -> BTreeMap<String, Value> {
        let mut state = BTreeMap::new();
        for event in &self.events {
            let Some(topic) = event.get("topic").and_then(Value::as_str) else {
                continue;
            };
            if matches!(topic, "alert" | "frame.saved" | "transfer.acked") {
                continue;
            }
            let Some(data) = event.get("data") else {
                continue;
            };
            state.insert(topic.to_owned(), data.clone());
        }
        state
    }

    /// The set of `frame_id`s the log says were saved, in order, deduplicated.
    ///
    /// Deduplicated because a node that restarts republishes nothing but *may* be asked to
    /// re-save; the assertion this feeds is about which frames exist, not about how many lines
    /// mention them.
    #[must_use]
    pub fn saved_frames(&self) -> Vec<String> {
        let mut frames: Vec<String> = Vec::new();
        for event in self.topic("frame.saved") {
            if let Some(id) = event
                .get("data")
                .and_then(|data| data.get("frame_id"))
                .and_then(Value::as_str)
            {
                if !frames.iter().any(|seen| seen == id) {
                    frames.push(id.to_owned());
                }
            }
        }
        frames
    }

    /// The `frame_id`s the log says the stacking server acknowledged.
    #[must_use]
    pub fn acked_frames(&self) -> Vec<String> {
        let mut frames: Vec<String> = Vec::new();
        for event in self.topic("transfer.acked") {
            if let Some(id) = event
                .get("data")
                .and_then(|data| data.get("frame_id"))
                .and_then(Value::as_str)
            {
                if !frames.iter().any(|seen| seen == id) {
                    frames.push(id.to_owned());
                }
            }
        }
        frames
    }

    /// Assert the envelope of every line: `v`, `ts`, `topic`, `data` and nothing else.
    ///
    /// # Panics
    ///
    /// On the first line that is not a well-formed §4.3 envelope, quoting it.
    pub fn assert_envelopes(&self) {
        for (index, event) in self.events.iter().enumerate() {
            let object = event
                .as_object()
                .unwrap_or_else(|| panic!("line {} is not a JSON object: {event}", index + 1));
            assert_eq!(
                object.get("v").and_then(Value::as_u64),
                Some(1),
                "line {} has no `v: 1`: {event}",
                index + 1
            );
            for key in ["ts", "topic"] {
                assert!(
                    object.get(key).and_then(Value::as_str).is_some(),
                    "line {} has no string `{key}`: {event}",
                    index + 1
                );
            }
            assert!(
                object.contains_key("data"),
                "line {} has no `data`: {event}",
                index + 1
            );
            let extra: Vec<&String> = object
                .keys()
                .filter(|key| !matches!(key.as_str(), "v" | "ts" | "topic" | "data"))
                .collect();
            assert!(
                extra.is_empty(),
                "line {} carries keys outside the §4.3 envelope: {extra:?}",
                index + 1
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fold keeps the latest value per topic and drops the ones that are not state. This is
    /// the one piece of this crate with logic worth testing without a container behind it.
    #[test]
    fn the_fold_is_latest_wins_and_skips_what_is_not_state() {
        let log = concat!(
            r#"{"v":1,"ts":"2026-07-30T21:00:00.000Z","topic":"mount.status","data":{"state":"idle"}}"#,
            "\n",
            r#"{"v":1,"ts":"2026-07-30T21:00:01.000Z","topic":"alert","data":{"code":"X"}}"#,
            "\n",
            r#"{"v":1,"ts":"2026-07-30T21:00:02.000Z","topic":"mount.status","data":{"state":"slewing"}}"#,
            "\n",
            r#"{"v":1,"ts":"2026-07-30T21:00:03.000Z","topic":"frame.saved","data":{"frame_id":"light_00001"}}"#,
            "\n",
        );
        let replay = Replay::parse(log);
        replay.assert_envelopes();
        assert!(replay.unparseable.is_empty());

        let state = replay.final_state();
        assert_eq!(state["mount.status"]["state"], "slewing");
        assert!(!state.contains_key("alert"), "an alert is not a state");
        assert!(!state.contains_key("frame.saved"), "a save is not a state");
        assert_eq!(replay.saved_frames(), vec!["light_00001".to_owned()]);
    }

    /// A truncated final line — what `kill -9` leaves — is reported, not silently dropped and not
    /// fatal to parsing the rest.
    #[test]
    fn a_truncated_tail_is_reported_and_the_rest_still_parses() {
        let log = concat!(
            r#"{"v":1,"ts":"2026-07-30T21:00:00.000Z","topic":"mount.status","data":{"state":"idle"}}"#,
            "\n",
            r#"{"v":1,"ts":"2026-07-30T21:00:02.000Z","topic":"mount.st"#,
        );
        let replay = Replay::parse(log);
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.unparseable.len(), 1);
        assert_eq!(replay.unparseable[0].0, 2);
    }
}
