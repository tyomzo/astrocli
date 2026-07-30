//! Latest-only frame delivery — SDD §5.3.1 (the camera thread's watch channel), §5.8.3, §8.3(6).
//!
//! Both image streams in this crate are *self-superseding*: a live-view frame the operator would
//! see two seconds late is worse than no frame, and a guide frame that old is worthless to a loop
//! correcting in the present. So the channel underneath keeps exactly one frame — the newest —
//! and a consumer slower than the producer silently skips the ones in between. That is the
//! designed behaviour, not a degradation: it is the same latest-only rule the WS hub applies to
//! `mount.position` (SDD §5.8.3), applied one layer lower so a slow consumer cannot make the
//! *driver* buffer.
//!
//! A consumer that must see every frame is asking for a different primitive and should drive
//! [`GuideCamera::capture_frame`](crate::guide::GuideCamera::capture_frame) in a loop instead.

use tokio::sync::watch;

/// The consumer half of a device's frame stream.
///
/// Cheap to clone: every clone is an independent cursor over the same single-slot channel, so
/// two consumers at different speeds do not affect each other (or the producer).
#[derive(Debug, Clone)]
pub struct FrameStream<T> {
    rx: watch::Receiver<Option<T>>,
}

/// The producer half, held by the driver.
///
/// Dropping it ends the stream: every [`FrameStream::next_frame`] then returns `None`. That is
/// how a driver signals "live view stopped" or "device disconnected" without a second channel.
#[derive(Debug)]
pub struct FrameSink<T> {
    tx: watch::Sender<Option<T>>,
}

impl<T: Clone> FrameStream<T> {
    /// Creates a producer/consumer pair.
    ///
    /// ```
    /// use astroctl_hal::stream::FrameStream;
    ///
    /// # let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    /// # runtime.block_on(async {
    /// let (sink, mut stream) = FrameStream::channel();
    ///
    /// // Three frames arrive while the consumer is busy elsewhere...
    /// sink.publish(1);
    /// sink.publish(2);
    /// sink.publish(3);
    ///
    /// // ...and it wakes to the newest one, not to a backlog of stale ones.
    /// assert_eq!(stream.next_frame().await, Some(3));
    ///
    /// drop(sink);
    /// assert_eq!(stream.next_frame().await, None);
    /// # });
    /// ```
    #[must_use]
    pub fn channel() -> (FrameSink<T>, Self) {
        let (tx, rx) = watch::channel(None);
        (FrameSink { tx }, Self { rx })
    }

    /// Waits for a frame newer than the last one this stream returned.
    ///
    /// Returns `None` once the driver has dropped its [`FrameSink`] — the stream has ended and
    /// will produce nothing further. Frames published while the caller was away are not queued;
    /// the next call yields the newest one (see the module docs).
    ///
    /// **Cancel-safe.** Dropping this future loses no frame that a later call would have
    /// returned, so it can be a `select!` arm — which is exactly how the WS hub consumes it.
    pub async fn next_frame(&mut self) -> Option<T> {
        loop {
            // `changed()` is the cancel-safe half; it resolves `Err` only when every sender is
            // gone. The loop exists for the initial `None` slot: a stream subscribed to before
            // the first frame sees one change with nothing in it.
            self.rx.changed().await.ok()?;
            let frame = self.rx.borrow_and_update().clone();
            if frame.is_some() {
                return frame;
            }
        }
    }

    /// The most recent frame without waiting, or `None` if the driver has published none yet.
    ///
    /// Does not consume: a following [`next_frame`](Self::next_frame) still waits for the *next*
    /// frame rather than returning this one again.
    #[must_use]
    pub fn latest(&self) -> Option<T> {
        self.rx.borrow().clone()
    }

    /// Another independent cursor on the same stream, positioned at the present — a subscriber
    /// gets the next frame published, never the one already in the slot.
    #[must_use]
    pub fn subscribe(&self) -> Self {
        let mut rx = self.rx.clone();
        rx.mark_unchanged();
        Self { rx }
    }
}

impl<T> FrameSink<T> {
    /// Publishes a frame, replacing whatever was in the slot.
    ///
    /// Never blocks, never fails, and never applies backpressure — a driver's acquisition loop
    /// must not be able to stall because a consumer is slow (SDD §5.8.3). Publishing with no
    /// consumers at all is equally fine; the frame simply becomes the one a late subscriber
    /// finds with [`FrameStream::latest`].
    pub fn publish(&self, frame: T) {
        drop(self.tx.send_replace(Some(frame)));
    }

    /// How many [`FrameStream`]s are currently attached.
    ///
    /// A driver may use this to skip generating frames nobody is watching — worth it when a
    /// frame costs a USB round trip or a synthetic star field.
    #[must_use]
    pub fn consumers(&self) -> usize {
        self.tx.receiver_count()
    }

    /// A new consumer, positioned at the present.
    #[must_use]
    pub fn subscribe(&self) -> FrameStream<T> {
        FrameStream {
            rx: self.tx.subscribe(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_slow_consumer_gets_the_newest_frame_not_a_backlog() {
        let (sink, mut stream) = FrameStream::channel();
        for frame in 1..=5 {
            sink.publish(frame);
        }
        assert_eq!(stream.next_frame().await, Some(5));

        sink.publish(6);
        assert_eq!(stream.next_frame().await, Some(6));
    }

    #[tokio::test]
    async fn next_frame_waits_for_a_frame_rather_than_returning_the_empty_slot() {
        let (sink, mut stream) = FrameStream::channel();
        let waiting = tokio::spawn(async move { stream.next_frame().await });

        // Give the reader a chance to park on the initial `None` before anything is published.
        tokio::task::yield_now().await;
        sink.publish("first");

        assert_eq!(waiting.await.expect("task completes"), Some("first"));
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_driver_drops_the_sink() {
        let (sink, mut stream) = FrameStream::channel();
        sink.publish(1);
        assert_eq!(stream.next_frame().await, Some(1));

        drop(sink);
        assert_eq!(stream.next_frame().await, None);
        // ...and stays ended, rather than parking forever.
        assert_eq!(stream.next_frame().await, None);
    }

    #[tokio::test]
    async fn a_subscriber_starts_at_the_present() {
        let (sink, stream) = FrameStream::channel();
        sink.publish("stale");

        let mut fresh = stream.subscribe();
        // The stale frame is readable without waiting...
        assert_eq!(fresh.latest(), Some("stale"));
        // ...but is not what the next wait resolves to.
        sink.publish("fresh");
        assert_eq!(fresh.next_frame().await, Some("fresh"));
    }

    #[tokio::test]
    async fn publishing_into_the_void_is_not_an_error() {
        let (sink, stream) = FrameStream::channel();
        assert_eq!(sink.consumers(), 1);
        drop(stream);
        assert_eq!(sink.consumers(), 0);

        // A camera thread must not fail because the last viewer closed the tab.
        sink.publish("nobody is listening");
        assert_eq!(sink.subscribe().latest(), Some("nobody is listening"));
    }
}
