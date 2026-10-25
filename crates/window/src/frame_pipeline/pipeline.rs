//! One capture's way to the wire: diff, route, encode, and queue each
//! frame, resynchronise on request, and settle a still picture once.

use crate::frame_pipeline::pick_codec;
use crate::frame_pipeline::should_use_video;
use crate::frame_pipeline::AdaptiveQuality;
use crate::frame_pipeline::FrameDiffer;
use crate::frame_pipeline::KeyframeClock;
use cursio_protocol::{Rect, TileCodec, WindowFrame};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameQueueError {
    Full,
    Closed,
}

/// Enqueue every packet belonging to one encoded update, or none of them.
///
/// H.264 encoders may return several packets for a single input frame. Sending
/// only the first packet before discovering that the bounded queue is full
/// would corrupt the decoder state, so capacity is reserved atomically.
pub fn try_queue_frames(
    tx: &mpsc::Sender<WindowFrame>,
    frames: Vec<WindowFrame>,
) -> Result<(), FrameQueueError> {
    if frames.is_empty() {
        return Ok(());
    }
    let permits = tx
        .try_reserve_many(frames.len())
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => FrameQueueError::Full,
            mpsc::error::TrySendError::Closed(()) => FrameQueueError::Closed,
        })?;
    for (permit, frame) in permits.zip(frames) {
        permit.send(frame);
    }
    Ok(())
}

/// What a capture backend's H.264 encoder offers the shared pipeline.
pub trait StreamEncoder {
    /// Encode one tightly packed BGRA frame at the encoder's size.
    fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, crate::WindowError>;
    /// Make the next encoded frame an IDR.
    fn force_keyframe(&mut self);
    /// Whether what this encoder produces loses detail, and so leaves the
    /// viewer a picture worth refining once the source stands still.
    fn lossy(&self) -> bool;
}

/// Opens and tunes a backend's encoders; what differs between platforms.
pub trait EncoderSource {
    type Encoder: StreamEncoder;

    /// Leave `slot` holding an encoder fit for a `width`x`height` frame at the
    /// quality the link allows now — reopening, retuning or leaving it as it
    /// is — and return the size it encodes, which may crop an odd edge.
    /// `None` when video cannot be used for this frame.
    fn prepare(
        &mut self,
        slot: &mut Option<Self::Encoder>,
        width: u32,
        height: u32,
        quality: &AdaptiveQuality,
    ) -> Option<(u32, u32)>;
}

/// What became of an update offered to the video path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VideoSend {
    Sent,
    /// The queue could not take it; the viewer is owed a resync.
    Dropped,
    /// No encoder for it; the caller falls back to tiles.
    Unavailable,
}

/// Most packets one full-picture resend may take, so it always fits the
/// capture queue ([`crate::CAPTURE_FRAME_QUEUE_CAPACITY`]) with room to spare,
/// and each band of a 4K picture stays under the transport's frame limit even
/// uncompressed.
pub(super) const FULL_PICTURE_BANDS: u32 = 4;

/// How long a source must stand still after lossy video before its final
/// picture is resent exactly.
///
/// Long enough that a pause between keystrokes or scroll steps is not taken
/// for the end of the motion — each refinement is a whole picture — and short
/// enough that the blur is gone before the eye settles on what stopped.
pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// Everything a capture backend does with a captured frame, and between them.
///
/// Both backends feed it full BGRA frames; it diffs them, routes each change to
/// a lossless tile or the video stream, and keeps what the viewer is owed when
/// no frame comes — which is the case that matters on a still screen. A viewer
/// that lost its place (a keyframe it never got, an update the queue could not
/// take) asks, or is marked, for a *resync*; [`idle`](Self::idle) answers it
/// from the last captured frame without waiting for the compositor to deliver
/// a new one, because on an unchanging desktop it never will.
pub struct CapturePipeline<S: EncoderSource> {
    id: cursio_protocol::WindowId,
    source: S,
    differ: FrameDiffer,
    video: Option<S::Encoder>,
    keyframes: KeyframeClock,
    /// The viewer lacks a picture to build on: it asked, or an update it needed
    /// was dropped.
    resync: bool,
    /// Lossy video reached the viewer and no exact picture has replaced it.
    unrefined: bool,
    /// When the source last changed.
    last_change: Option<std::time::Instant>,
    /// Nobody takes frames any more: the session dropped this capture.
    closed: bool,
    seq: u32,
    quality: std::sync::Arc<AdaptiveQuality>,
    last_seq: std::sync::Arc<std::sync::atomic::AtomicU32>,
    frames: mpsc::Sender<WindowFrame>,
}

impl<S: EncoderSource> CapturePipeline<S> {
    pub fn new(
        id: cursio_protocol::WindowId,
        source: S,
        quality: std::sync::Arc<AdaptiveQuality>,
        last_seq: std::sync::Arc<std::sync::atomic::AtomicU32>,
        frames: mpsc::Sender<WindowFrame>,
    ) -> Self {
        Self {
            id,
            source,
            differ: FrameDiffer::new(),
            video: None,
            keyframes: KeyframeClock::default(),
            resync: false,
            unrefined: false,
            last_change: None,
            closed: false,
            seq: 0,
            quality,
            last_seq,
            frames,
        }
    }

    /// The viewer asked for a picture it can start from.
    pub fn request_resync(&mut self) {
        self.resync = true;
    }

    /// Whether the frames' receiver is gone, so the capture should end.
    ///
    /// A full queue and a closed one are opposite cases. Full means the
    /// viewer is owed a resync, so the encoder is dropped and the next frame
    /// opens a fresh one on a keyframe. Closed means there is no viewer:
    /// treating it as full made a replaced capture reopen NVENC — a GPU
    /// session and a keyframe for nobody — in the moment between the session
    /// dropping it and its thread reading the stop command.
    pub fn closed(&self) -> bool {
        self.closed
    }

    fn mark_closed(&mut self) {
        self.closed = true;
        self.video = None;
    }

    /// One captured frame, tightly packed BGRA.
    pub fn frame(&mut self, frame: &[u8], width: u32, height: u32, now: std::time::Instant) {
        if self.closed {
            return;
        }
        let dirty = self.differ.diff(frame, width, height);
        if dirty.is_some() {
            self.last_change = Some(now);
        }
        if self.resync {
            self.send_resync(frame, width, height, now);
            return;
        }
        let Some(dirty) = dirty else {
            return; // unchanged — a static window costs nothing
        };
        // A small change rides a lossless tile; a large one goes through the
        // video encoder, which codes the difference from the previous frame.
        if should_use_video(dirty, width, height, self.quality.threshold())
            && self.send_video(frame, width, height, false, now) != VideoSend::Unavailable
        {
            return;
        }
        self.send_tile(frame, width, dirty);
    }

    /// Between frames: answer what the viewer is owed from the last frame.
    ///
    /// Besides a resync, that is a *refinement*. A lossy stream spends few
    /// bits on each frame of motion because the next frame will correct it —
    /// but when the source stops, no next frame comes, and the last one's
    /// blur stays: a closed menu left a faint imprint where it had been. So
    /// once lossy video has stood still for [`SETTLE`], the final picture is
    /// sent once more as lossless tiles. Tiles rather than a better IDR: they
    /// are exact, need no encoder retuned for one frame, and a viewer blits
    /// them over whatever its decoder left. Each video frame covers the whole
    /// window, so the whole window is what needs replacing. Once per settle:
    /// only new lossy video arms it again.
    pub fn idle(&mut self, now: std::time::Instant) {
        if self.closed {
            return;
        }
        let settled = self.unrefined
            && self
                .last_change
                .is_some_and(|at| now.saturating_duration_since(at) >= SETTLE);
        if !self.resync && !settled {
            return;
        }
        let Some((frame, width, height)) = self.differ.last() else {
            // Nothing captured yet; the first frame is a full one anyway.
            return;
        };
        let frame = frame.to_vec();
        if self.resync {
            self.send_resync(&frame, width, height, now);
        } else if self.send_full_picture(&frame, width, height) {
            self.unrefined = false;
        }
    }

    /// The whole picture, as an IDR where an encoder opens and as lossless
    /// tiles where none does.
    fn send_resync(&mut self, frame: &[u8], width: u32, height: u32, now: std::time::Instant) {
        match self.send_video(frame, width, height, true, now) {
            VideoSend::Sent => self.resync = false,
            VideoSend::Dropped => {}
            VideoSend::Unavailable => {
                if self.send_full_picture(frame, width, height) {
                    self.resync = false;
                    self.unrefined = false;
                }
            }
        }
    }

    /// The number the `n`th frame from now will carry, without spending it.
    fn seq_ahead(&self, n: u32) -> u32 {
        self.seq.wrapping_add(n)
    }

    /// Queue one update whole or not at all, and only then spend its numbers.
    ///
    /// The host reads the link from how far acks trail the last number spent,
    /// so a number spent on a frame the queue refused read as a frame the
    /// link had failed to carry — a backlog made of nothing.
    fn queue(&mut self, frames: Vec<WindowFrame>) -> Result<(), FrameQueueError> {
        let last = frames.last().map(|f| f.seq);
        try_queue_frames(&self.frames, frames)?;
        if let Some(seq) = last {
            let now = std::time::Instant::now();
            for n in self.seq.wrapping_add(1)..=seq {
                self.quality.note_queued(n, now);
            }
            self.seq = seq;
            self.last_seq
                .store(seq, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    fn send_video(
        &mut self,
        frame: &[u8],
        width: u32,
        height: u32,
        keyframe: bool,
        now: std::time::Instant,
    ) -> VideoSend {
        let Some((vw, vh)) = self
            .source
            .prepare(&mut self.video, width, height, &self.quality)
        else {
            return VideoSend::Unavailable;
        };
        let Some(enc) = self.video.as_mut() else {
            return VideoSend::Unavailable;
        };
        if keyframe || self.keyframes.due(now) {
            enc.force_keyframe();
        }
        let cropped;
        let input = if (vw, vh) == (width, height) {
            frame
        } else {
            cropped = crop_frame(frame, width, vw, vh);
            &cropped
        };
        let packets = match enc.encode(input) {
            Ok(packets) => packets,
            Err(e) => {
                // Dropped so the next frame retries cleanly rather than
                // repeating a broken session.
                tracing::warn!(error = %e, "video encode failed; falling back to tiles");
                self.video = None;
                return VideoSend::Unavailable;
            }
        };
        self.keyframes.note_sent(&packets, now);
        let refine = !packets.is_empty() && enc.lossy();
        let full = Rect {
            x: 0,
            y: 0,
            width: vw,
            height: vh,
        };
        let frames = packets
            .into_iter()
            .zip(1..)
            .map(|(data, n)| WindowFrame {
                id: self.id,
                seq: self.seq_ahead(n),
                dirty: full,
                codec: TileCodec::LossyVideo,
                data,
            })
            .collect();
        match self.queue(frames) {
            Ok(()) => {
                self.quality.note_video_sent();
                self.unrefined |= refine;
                VideoSend::Sent
            }
            Err(FrameQueueError::Full) => {
                // The viewer never saw this encoder state; the next encoder
                // opens on a keyframe, and the resync makes it the whole
                // picture even if the screen stops changing here.
                self.video = None;
                self.resync = true;
                VideoSend::Dropped
            }
            Err(FrameQueueError::Closed) => {
                self.mark_closed();
                VideoSend::Dropped
            }
        }
    }

    fn send_tile(&mut self, frame: &[u8], width: u32, dirty: Rect) {
        let tile = FrameDiffer::extract(frame, width, dirty);
        let codec = pick_codec(tile.len());
        let data = match crate::codec::encode_tile(codec, &tile) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(error = %e, "tile encode failed; dropping frame");
                self.resync = true;
                return;
            }
        };
        // Never block the capture thread: a full queue loses this delta, and
        // the resync replaces it with the whole picture.
        let tile = WindowFrame {
            id: self.id,
            seq: self.seq_ahead(1),
            dirty,
            codec,
            data,
        };
        match self.queue(vec![tile]) {
            Ok(()) => {}
            Err(FrameQueueError::Full) => self.resync = true,
            Err(FrameQueueError::Closed) => self.mark_closed(),
        }
    }

    /// The whole picture as lossless tiles, in horizontal bands; whether it
    /// was queued.
    fn send_full_picture(&mut self, frame: &[u8], width: u32, height: u32) -> bool {
        if height == 0 || width == 0 {
            return false;
        }
        let band = height.div_ceil(FULL_PICTURE_BANDS);
        let rows: Vec<(u32, u32)> = (0..height)
            .step_by(band as usize)
            .map(|y| (y, band.min(height - y)))
            .collect();
        // Checked before compressing: a busy queue would refuse the result,
        // and zstd on a whole picture is not work to throw away.
        if self.frames.capacity() < rows.len() {
            return false;
        }
        let seq = self.seq_ahead(1);
        let mut frames = Vec::with_capacity(rows.len());
        for (y, h) in rows {
            let dirty = Rect {
                x: 0,
                y,
                width,
                height: h,
            };
            let tile = FrameDiffer::extract(frame, width, dirty);
            let codec = pick_codec(tile.len());
            match crate::codec::encode_tile(codec, &tile) {
                Ok(data) => frames.push(WindowFrame {
                    id: self.id,
                    seq,
                    dirty,
                    codec,
                    data,
                }),
                Err(e) => {
                    tracing::warn!(error = %e, "could not encode the whole picture");
                    return false;
                }
            }
        }
        match self.queue(frames) {
            Ok(()) => true,
            Err(FrameQueueError::Full) => false,
            Err(FrameQueueError::Closed) => {
                self.mark_closed();
                false
            }
        }
    }
}

/// The top-left `vw`x`vh` of a tightly packed BGRA frame `width` pixels wide.
pub(super) fn crop_frame(frame: &[u8], width: u32, vw: u32, vh: u32) -> Vec<u8> {
    let (src_stride, dst_stride) = (width as usize * 4, vw as usize * 4);
    let mut out = Vec::with_capacity(dst_stride * vh as usize);
    for row in 0..vh as usize {
        let start = row * src_stride;
        out.extend_from_slice(&frame[start..start + dst_stride]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_pipeline::AdaptiveQuality;
    use crate::frame_pipeline::KEYFRAME_INTERVAL;

    #[test]
    fn packet_batch_is_enqueued_atomically() {
        fn packet(seq: u32) -> WindowFrame {
            WindowFrame {
                id: 1,
                seq,
                dirty: Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                codec: TileCodec::LossyVideo,
                data: vec![seq as u8],
            }
        }

        let (tx, mut rx) = mpsc::channel(2);
        assert_eq!(try_queue_frames(&tx, vec![packet(1), packet(2)]), Ok(()));
        assert_eq!(
            try_queue_frames(&tx, vec![packet(3)]),
            Err(FrameQueueError::Full)
        );
        assert_eq!(rx.try_recv().unwrap().seq, 1);
        assert_eq!(rx.try_recv().unwrap().seq, 2);
        assert!(rx.try_recv().is_err());
    }

    use std::time::{Duration, Instant};

    /// Emits an IDR-shaped packet when forced or first, a P-shaped one else.
    struct FakeEncoder {
        forced: bool,
        started: bool,
    }

    impl StreamEncoder for FakeEncoder {
        fn encode(&mut self, _bgra: &[u8]) -> Result<Vec<Vec<u8>>, crate::WindowError> {
            let key = std::mem::take(&mut self.forced) || !self.started;
            self.started = true;
            Ok(vec![if key {
                vec![0, 0, 0, 1, 0x65]
            } else {
                vec![0, 0, 0, 1, 0x41]
            }])
        }

        fn force_keyframe(&mut self) {
            self.forced = true;
        }

        fn lossy(&self) -> bool {
            true
        }
    }

    /// Opens a [`FakeEncoder`], or refuses to when `available` is false.
    struct FakeSource {
        available: bool,
        opened: usize,
    }

    impl EncoderSource for FakeSource {
        type Encoder = FakeEncoder;

        fn prepare(
            &mut self,
            slot: &mut Option<FakeEncoder>,
            width: u32,
            height: u32,
            _quality: &AdaptiveQuality,
        ) -> Option<(u32, u32)> {
            if !self.available {
                return None;
            }
            if slot.is_none() {
                self.opened += 1;
            }
            slot.get_or_insert(FakeEncoder {
                forced: false,
                started: false,
            });
            Some((width, height))
        }
    }

    const W: u32 = 16;
    const H: u32 = 8;

    fn pipeline(
        available: bool,
        capacity: usize,
    ) -> (CapturePipeline<FakeSource>, mpsc::Receiver<WindowFrame>) {
        let (tx, rx) = mpsc::channel(capacity);
        let p = CapturePipeline::new(
            7,
            FakeSource {
                available,
                opened: 0,
            },
            Default::default(),
            Default::default(),
            tx,
        );
        (p, rx)
    }

    fn solid(c: u8) -> Vec<u8> {
        vec![c; (W * H * 4) as usize]
    }

    fn drain(rx: &mut mpsc::Receiver<WindowFrame>) -> Vec<WindowFrame> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn is_idr(frame: &WindowFrame) -> bool {
        frame.codec == TileCodec::LossyVideo && crate::video::contains_keyframe(&frame.data)
    }

    #[test]
    fn a_still_screen_owes_nothing_however_long_it_idles() {
        // Tiles only: lossy video would owe its one refinement, tested below.
        let (mut p, mut rx) = pipeline(false, 8);
        let now = Instant::now();
        p.frame(&solid(1), W, H, now);
        drain(&mut rx);
        for i in 1..100 {
            p.idle(now + Duration::from_millis(16 * i));
        }
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_keyframe_request_on_a_still_screen_reencodes_the_last_frame_at_once() {
        let (mut p, mut rx) = pipeline(true, 8);
        let now = Instant::now();
        p.frame(&solid(1), W, H, now);
        p.frame(&solid(2), W, H, now + Duration::from_millis(10));
        drain(&mut rx);

        p.request_resync();
        p.idle(now + Duration::from_millis(20));
        let sent = drain(&mut rx);
        assert_eq!(sent.len(), 1);
        assert!(
            is_idr(&sent[0]),
            "a resync must be a picture a decoder can enter on"
        );
        // Answered once, not on every tick after.
        p.idle(now + Duration::from_millis(40));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn with_no_encoder_a_request_is_answered_with_the_whole_picture_as_tiles() {
        let (mut p, mut rx) = pipeline(false, 8);
        let now = Instant::now();
        p.frame(&solid(1), W, H, now);
        drain(&mut rx);
        p.request_resync();
        p.idle(now);
        let sent = drain(&mut rx);
        assert!(!sent.is_empty());
        assert!(sent.iter().all(|f| f.codec.is_lossless()));
        let rows: u32 = sent.iter().map(|f| f.dirty.height).sum();
        assert_eq!(rows, H, "the bands must cover the whole picture");
    }

    #[test]
    fn an_update_the_queue_could_not_take_is_made_good_once_there_is_room() {
        let (mut p, mut rx) = pipeline(true, 1);
        let now = Instant::now();
        p.frame(&solid(1), W, H, now);
        // The queue is full: this change is lost.
        p.frame(&solid(2), W, H, now + Duration::from_millis(10));
        assert_eq!(drain(&mut rx).len(), 1);

        // The screen then stays still; the idle tick still delivers it.
        p.idle(now + Duration::from_millis(20));
        let sent = drain(&mut rx);
        assert_eq!(sent.len(), 1);
        assert!(is_idr(&sent[0]));
    }

    /// Numbers go only to frames the queue took, so the gap between the last
    /// number and the viewer's ack counts frames that are actually on their
    /// way — not refusals, which never left this machine.
    #[test]
    fn a_frame_the_queue_refused_spends_no_number() {
        let (mut p, mut rx) = pipeline(true, 1);
        let now = Instant::now();
        p.frame(&solid(1), W, H, now);
        p.frame(&solid(2), W, H, now + Duration::from_millis(10));
        let first: Vec<u32> = drain(&mut rx).iter().map(|f| f.seq).collect();
        assert_eq!(first, vec![1]);

        p.idle(now + Duration::from_millis(20));
        let next: Vec<u32> = drain(&mut rx).iter().map(|f| f.seq).collect();
        assert_eq!(next, vec![2]);
        assert_eq!(p.last_seq.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn a_video_frame_after_the_interval_is_forced_to_a_keyframe() {
        let (mut p, mut rx) = pipeline(true, 8);
        let start = Instant::now();
        p.frame(&solid(1), W, H, start);
        p.frame(&solid(2), W, H, start + Duration::from_millis(500));
        p.frame(&solid(3), W, H, start + KEYFRAME_INTERVAL);
        let keys: Vec<bool> = drain(&mut rx).iter().map(is_idr).collect();
        assert_eq!(keys, vec![true, false, true]);
    }

    #[test]
    fn lossy_video_that_stands_still_is_refined_exactly_once() {
        let (mut p, mut rx) = pipeline(true, 8);
        let start = Instant::now();
        p.frame(&solid(1), W, H, start);
        p.frame(&solid(2), W, H, start + Duration::from_millis(16));
        drain(&mut rx);

        // Still moving, as far as the settle window can tell.
        p.idle(start + Duration::from_millis(100));
        assert!(drain(&mut rx).is_empty());

        let settled = start + Duration::from_millis(16) + SETTLE;
        p.idle(settled);
        let refined = drain(&mut rx);
        assert!(!refined.is_empty());
        assert!(refined.iter().all(|f| f.codec.is_lossless()));
        let rows: u32 = refined.iter().map(|f| f.dirty.height).sum();
        assert_eq!(rows, H, "the refinement must cover the whole picture");

        // A still screen never loops.
        for i in 1..50 {
            p.idle(settled + Duration::from_millis(16 * i));
        }
        assert!(drain(&mut rx).is_empty());

        // Only new lossy video arms it again.
        let later = settled + Duration::from_secs(1);
        p.frame(&solid(3), W, H, later);
        drain(&mut rx);
        p.idle(later + SETTLE);
        assert!(!drain(&mut rx).is_empty());
    }

    /// A replaced capture's receiver goes before its thread reads the stop
    /// command. Read as a full queue, that reopened NVENC for nobody: a GPU
    /// session and a keyframe, logged on the host as a second "video encoder
    /// opened" 200 ms after the new source's.
    #[test]
    fn a_capture_nobody_receives_from_stops_instead_of_reopening_its_encoder() {
        let (mut p, rx) = pipeline(true, 8);
        let now = Instant::now();
        p.frame(&solid(1), W, H, now);
        p.frame(&solid(2), W, H, now + Duration::from_millis(10));
        drop(rx);

        p.frame(&solid(3), W, H, now + Duration::from_millis(20));
        assert!(p.closed());
        assert!(p.video.is_none(), "the encoder must be released at once");

        p.request_resync();
        p.idle(now + Duration::from_millis(30));
        p.frame(&solid(4), W, H, now + Duration::from_millis(40));
        p.idle(now + Duration::from_secs(5));
        assert_eq!(
            p.source.opened, 1,
            "no encoder may open after the viewer left"
        );
    }

    #[test]
    fn a_tile_only_capture_notices_its_viewer_leaving() {
        let (mut p, rx) = pipeline(false, 8);
        let now = Instant::now();
        p.frame(&solid(1), W, H, now);
        drop(rx);
        p.frame(&solid(2), W, H, now + Duration::from_millis(10));
        assert!(p.closed());
    }

    #[test]
    fn a_screen_that_only_changed_through_tiles_needs_no_refinement() {
        let (mut p, mut rx) = pipeline(false, 8);
        let start = Instant::now();
        p.frame(&solid(1), W, H, start);
        p.frame(&solid(2), W, H, start + Duration::from_millis(16));
        drain(&mut rx);
        p.idle(start + Duration::from_secs(5));
        assert!(drain(&mut rx).is_empty());
    }
}
