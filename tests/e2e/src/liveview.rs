//! The binary image sockets — `/ws/liveview` on the field node and `/ws/preview` on the stack,
//! the latter reached through the proxy at `/stack/ws/preview`.
//!
//! One envelope serves both (`astroctl-core`'s `image_frame`), which is why one parser serves both
//! here. Written out by hand from the wire description rather than shared with the server: the
//! magic, the version byte and the big-endian length are exactly the kind of thing a refactor can
//! change without any test noticing, if the test uses the same encoder.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::task::JoinHandle;

/// `ACLV` — the four bytes every image frame starts with.
const MAGIC: &[u8; 4] = b"ACLV";
/// The protocol version byte this suite understands. A bump here is a wire change, and this
/// assertion is where a client finds out about it.
const PROTOCOL_VERSION: u8 = 1;
/// `kind` byte values.
pub const KIND_LIVE: u8 = 0;
pub const KIND_PREVIEW: u8 = 1;

/// One decoded image frame.
#[derive(Debug, Clone)]
pub struct ImageFrame {
    pub kind: u8,
    /// The server's timestamp from the meta object.
    pub ts: String,
    /// Present on preview frames, absent on live ones — the key is omitted, not null.
    pub frame_id: Option<String>,
    pub jpeg_len: usize,
    /// When this client saw it.
    pub at: Instant,
}

/// Decode one frame off the wire.
///
/// # Panics
///
/// On anything that is not a well-formed frame, quoting what was actually there. Every caller is
/// an assertion about the protocol, so a soft failure would only move the panic somewhere less
/// informative.
#[must_use]
pub fn parse(bytes: &[u8], at: Instant) -> ImageFrame {
    assert!(
        bytes.len() >= 8,
        "an image frame is at least 8 bytes of header, got {}",
        bytes.len()
    );
    assert_eq!(&bytes[0..4], MAGIC, "an image frame starts with ACLV");
    assert_eq!(
        bytes[4], PROTOCOL_VERSION,
        "this suite reads image protocol v{PROTOCOL_VERSION}"
    );
    let kind = bytes[5];
    let meta_len = usize::from(u16::from_be_bytes([bytes[6], bytes[7]]));
    assert!(
        bytes.len() >= 8 + meta_len,
        "the frame declares {meta_len} bytes of meta but holds {}",
        bytes.len() - 8
    );
    let meta: Value = serde_json::from_slice(&bytes[8..8 + meta_len])
        .unwrap_or_else(|error| panic!("the meta object is not JSON ({error})"));
    ImageFrame {
        kind,
        ts: meta
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        frame_id: meta
            .get("frame_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        jpeg_len: bytes.len() - 8 - meta_len,
        at,
    }
}

/// A recording of one binary image socket.
pub struct FrameSocket {
    frames: Arc<Mutex<Vec<ImageFrame>>>,
    closed: Arc<Mutex<Option<String>>>,
    reader: JoinHandle<()>,
}

impl FrameSocket {
    /// Open `/ws/liveview` on the field node.
    ///
    /// # Panics
    ///
    /// When the upgrade fails.
    pub async fn liveview(client: &crate::Client) -> Self {
        Self::open(client, "/ws/liveview").await
    }

    /// Open the stacking server's preview socket **through the proxy**, the way the PWA does.
    ///
    /// `/stack/ws/preview`, not the stack node's own address: the stack publishes no port, and the
    /// proxy spending the field node's ticket and re-dialling upstream with its own bearer
    /// credential is the mechanism ADR-07 is about. Testing it any other way would test a topology
    /// nothing in this system has.
    ///
    /// # Panics
    ///
    /// When the upgrade fails.
    pub async fn stack_preview(client: &crate::Client) -> Self {
        Self::open(client, "/stack/ws/preview").await
    }

    async fn open(client: &crate::Client, path: &str) -> Self {
        // A ticket per socket. `/ws`, `/ws/liveview` and `/stack/ws/*` each consume their own;
        // reusing one opens the first and fails the second, which is the single-use rule working.
        let ticket = client.ws_ticket().await;
        let url = format!(
            "{}{path}?ticket={ticket}",
            client.base().replacen("http://", "ws://", 1)
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap_or_else(|error| panic!("cannot open {url}: {error}"));

        let frames = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&frames);
        let closed_sink = Arc::clone(&closed);
        let reader = tokio::spawn(async move {
            while let Some(frame) = socket.next().await {
                match frame {
                    Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes)) => {
                        let decoded = parse(&bytes, Instant::now());
                        sink.lock().expect("frame lock").push(decoded);
                    }
                    // Text, ping, pong and close all mean the same thing here: not an image. The
                    // socket is one-way and binary-only, so anything else is the server saying
                    // goodbye, and the loop's own exit records that.
                    Ok(_) => {}
                    Err(error) => {
                        *closed_sink.lock().expect("close lock") = Some(error.to_string());
                        return;
                    }
                }
            }
            *closed_sink.lock().expect("close lock") = Some("stream ended".to_owned());
        });

        Self {
            frames,
            closed,
            reader,
        }
    }

    /// Everything received so far.
    #[must_use]
    pub fn frames(&self) -> Vec<ImageFrame> {
        self.frames.lock().expect("frame lock").clone()
    }

    /// How many frames have arrived — the cheap form for a saturation scenario that only needs to
    /// know the link is busy.
    #[must_use]
    pub fn count(&self) -> usize {
        self.frames.lock().expect("frame lock").len()
    }

    /// Why the socket closed, if it has.
    #[must_use]
    pub fn closed(&self) -> Option<String> {
        self.closed.lock().expect("close lock").clone()
    }

    /// Wait for a preview frame naming `frame_id`, returning how long it took from `since`.
    ///
    /// # Panics
    ///
    /// On timeout, listing what did arrive.
    pub async fn wait_for_frame_id(
        &self,
        frame_id: &str,
        since: Instant,
        timeout: std::time::Duration,
    ) -> std::time::Duration {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(found) = self
                .frames()
                .into_iter()
                .find(|frame| frame.frame_id.as_deref() == Some(frame_id))
            {
                return found.at.duration_since(since);
            }
            assert!(
                Instant::now() < deadline,
                "no preview for {frame_id} within {timeout:?}; received {:?}",
                self.frames()
                    .iter()
                    .map(|frame| frame.frame_id.clone())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

impl Drop for FrameSocket {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
