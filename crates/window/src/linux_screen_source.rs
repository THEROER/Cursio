//! A peer's screen source as a PipeWire video node on this machine — no window.
//!
//! On GNOME a screen-sharing application (Discord, OBS, a browser) can take
//! pixels only from what the ScreenCast portal hands it, and what the portal
//! hands out is a PipeWire node id. So a remote-screen view here is not a
//! window at all: it is a `Video/Source` node fed with the peer's decoded
//! frames, and Cursio's portal backend (`linux_portal`) returns its id when
//! the user picks that peer source in the capture app's own dialog. Nothing
//! is drawn on this desktop, which was the whole point — a window that only
//! existed to be captured was rendering for nobody.
//!
//! ## Shape
//!
//! It is a [`Surrogate`], so the remote-screen state machine drives it exactly
//! like a window: tiles and H.264 packets in, presentation acks out. The node
//! lives on a PipeWire thread of its own, which also owns the H.264 decoder
//! (FFmpeg's scaler is not `Send`, the same reason the GTK surrogate decodes on
//! its thread). The picture is kept in a [`Framebuffer`] at the source's size
//! and copied into each buffer the node sends.
//!
//! The node's size is fixed when it is created — the source's size, from the
//! host's `SourceShown` — because renegotiating a live stream is a
//! consumer-by-consumer gamble (OBS follows, a browser's WebRTC capturer may
//! not). A source that changes size afterwards (a window resized on the host)
//! is letterboxed into it, the same as the view windows were.
//!
//! The node is a **driver**: nothing else in the graph clocks a screen source,
//! so it pushes a buffer when the picture changes and not otherwise — an idle
//! desktop costs no buffers at all, which is what consumers of the GNOME
//! portal already expect (Mutter's own screen-cast nodes behave the same).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use cursio_protocol::{
    Rect, TileCodec, WindowCloseReason, WindowFrame, WindowFrameAck, WindowId, WindowInput,
    WindowMeta,
};
use tokio::sync::mpsc;

use crate::codec::decode_tile;
use crate::framebuffer::{geometry_of, scale_into, Framebuffer, Geometry};
use crate::presentation_ack::PresentationAcks;
use crate::video_decoder::VideoDecoder;
use crate::{Surrogate, WindowError};

/// What the session hands the PipeWire thread.
enum Command {
    Frame { seq: u32, dirty: Rect, raw: Vec<u8> },
    VideoPacket { seq: u32, packet: Vec<u8> },
    Grow { width: u32, height: u32 },
    Resize { width: u32, height: u32 },
    Restart,
    Stop,
}

/// A remote-screen source published as a PipeWire node.
pub struct PipeWireScreenSource {
    id: WindowId,
    node_id: u32,
    width: u32,
    height: u32,
    commands: std_mpsc::Sender<Command>,
    presented: mpsc::UnboundedReceiver<u32>,
    acks: PresentationAcks,
    /// Never sent on: a node has no user input to forward. Kept so the channel
    /// reads as open — a closed one is how a view reports "the user closed
    /// me", and nothing closes a node but its session.
    input: mpsc::UnboundedReceiver<WindowInput>,
    _input_tx: mpsc::UnboundedSender<WindowInput>,
    ended: Arc<AtomicBool>,
}

impl PipeWireScreenSource {
    /// Create the node for a source of `meta.width` × `meta.height`, named
    /// after `meta.title`, and wait until PipeWire has given it an id.
    pub fn open(meta: &WindowMeta) -> Result<Self, WindowError> {
        if meta.width == 0 || meta.height == 0 {
            return Err(WindowError::Backend(
                "a screen source needs its size before it can be published".into(),
            ));
        }
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let (presented_tx, presented_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let ended = Arc::new(AtomicBool::new(false));
        let thread_meta = meta.clone();
        let thread_ended = ended.clone();
        std::thread::Builder::new()
            .name("cursio-screen-node".into())
            .spawn(move || {
                if let Err(e) = run(thread_meta, cmd_rx, presented_tx, &ready_tx) {
                    tracing::warn!(error = %e, "remote-screen node ended");
                    let _ = ready_tx.send(Err(e));
                }
                thread_ended.store(true, Ordering::Relaxed);
            })
            .map_err(|e| WindowError::Backend(format!("spawn screen node thread: {e}")))?;
        let node_id = match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => return Err(WindowError::Backend(e)),
            Err(_) => {
                let _ = cmd_tx.send(Command::Stop);
                return Err(WindowError::Backend(
                    "PipeWire did not create the screen node; is the PipeWire daemon running?"
                        .into(),
                ));
            }
        };
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        tracing::info!(node_id, title = %meta.title, "remote-screen source published to PipeWire");
        Ok(Self {
            id: meta.id,
            node_id,
            width: meta.width,
            height: meta.height,
            commands: cmd_tx,
            presented: presented_rx,
            acks: PresentationAcks::new(meta.id),
            input: input_rx,
            _input_tx: input_tx,
            ended,
        })
    }

    /// The PipeWire node id a capture app connects to.
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    fn send(&self, command: Command) -> Result<(), WindowError> {
        if self.ended.load(Ordering::Relaxed) {
            return Err(WindowError::Backend("the screen node has ended".into()));
        }
        self.commands
            .send(command)
            .map_err(|_| WindowError::Backend("the screen node has ended".into()))
    }
}

impl Surrogate for PipeWireScreenSource {
    fn apply_frame(&mut self, frame: &WindowFrame) -> Result<(), WindowError> {
        match geometry_of(frame.dirty, frame.codec, self.width, self.height) {
            Geometry::Unchanged => {}
            Geometry::Resize { width, height } => {
                (self.width, self.height) = (width, height);
                self.send(Command::Resize { width, height })?;
            }
            Geometry::Grow { width, height } => {
                (self.width, self.height) = (width, height);
                self.send(Command::Grow { width, height })?;
            }
        }
        if frame.codec == TileCodec::LossyVideo {
            return self.send(Command::VideoPacket {
                seq: frame.seq,
                packet: frame.data.clone(),
            });
        }
        let expected = (frame.dirty.width as usize) * (frame.dirty.height as usize) * 4;
        let raw = decode_tile(frame.codec, &frame.data, expected)?;
        self.send(Command::Frame {
            seq: frame.seq,
            dirty: frame.dirty,
            raw,
        })
    }

    fn input(&mut self) -> &mut mpsc::UnboundedReceiver<WindowInput> {
        &mut self.input
    }

    fn take_ack(&mut self) -> Option<WindowFrameAck> {
        while let Ok(seq) = self.presented.try_recv() {
            self.acks.note_presented(seq);
        }
        self.acks.take()
    }

    fn node_id(&self) -> Option<u32> {
        Some(self.node_id)
    }

    fn restart_stream(&mut self, _title: &str) {
        self.acks = PresentationAcks::new(self.id);
        let _ = self.send(Command::Restart);
    }

    fn close(&mut self, reason: WindowCloseReason) {
        tracing::info!(
            ?reason,
            node_id = self.node_id,
            "withdrawing remote-screen node"
        );
        let _ = self.commands.send(Command::Stop);
    }
}

impl Drop for PipeWireScreenSource {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
    }
}

/// The formats offered: BGRx and BGRA, which are the wire format with and
/// without meaning in the alpha byte — no conversion either way — at exactly
/// the node's size, at whatever rate frames arrive up to 60 per second.
fn format_pod(width: u32, height: u32) -> Vec<u8> {
    use pipewire::spa::{
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
            ParamType,
        },
        pod::{object, property, serialize::PodSerializer, Value},
        utils::{Fraction, Rectangle, SpaTypes},
    };
    let obj = object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction { num: 60, denom: 1 }
        ),
    );
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serializing a static format pod cannot fail")
        .0
        .into_inner()
}

/// Buffer requirements, answered once the format is fixed. Shared memory
/// (`MemFd`) because the consumer is another process; without a stated size
/// the server has nothing to allocate.
fn buffers_pod(width: u32, height: u32) -> Vec<u8> {
    use pipewire::spa::{
        param::ParamType,
        pod::{serialize::PodSerializer, Object, Property, PropertyFlags, Value},
        sys,
        utils::SpaTypes,
    };
    let stride = (width * 4) as i32;
    let int = |key, value| Property {
        key,
        flags: PropertyFlags::empty(),
        value: Value::Int(value),
    };
    let obj = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            int(sys::SPA_PARAM_BUFFERS_buffers, 4),
            int(sys::SPA_PARAM_BUFFERS_blocks, 1),
            int(sys::SPA_PARAM_BUFFERS_size, stride * height as i32),
            int(sys::SPA_PARAM_BUFFERS_stride, stride),
            int(sys::SPA_PARAM_BUFFERS_dataType, 1 << sys::SPA_DATA_MemFd),
        ],
    };
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serializing a static buffers pod cannot fail")
        .0
        .into_inner()
}

/// The PipeWire thread: owns the node, the decoder and the picture.
fn run(
    meta: WindowMeta,
    commands: std_mpsc::Receiver<Command>,
    presented: mpsc::UnboundedSender<u32>,
    ready: &std_mpsc::Sender<Result<u32, String>>,
) -> Result<(), String> {
    use std::cell::RefCell;
    use std::rc::Rc;

    use pipewire::{
        context::ContextRc,
        main_loop::MainLoopRc,
        properties::properties,
        spa::{pod::Pod, utils::Direction},
        stream::{StreamFlags, StreamRc},
    };

    pipewire::init();
    let (node_w, node_h) = (meta.width, meta.height);
    let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect to PipeWire: {e}"))?;
    let stream = StreamRc::new(
        core,
        "cursio-remote-screen",
        properties! {
            *pipewire::keys::MEDIA_CLASS => "Video/Source",
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Source",
            *pipewire::keys::MEDIA_ROLE => "Screen",
            *pipewire::keys::NODE_NAME => "cursio-remote-screen",
            *pipewire::keys::NODE_DESCRIPTION => meta.title.as_str(),
        },
    )
    .map_err(|e| format!("stream: {e}"))?;

    // The picture as it should go out, at the node's size; rewritten on the
    // loop thread, copied out in `process`.
    let outgoing = Rc::new(RefCell::new(vec![0u8; (node_w * node_h * 4) as usize]));
    let for_process = outgoing.clone();
    let for_params = stream.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_, (), id, param| {
            if param.is_none() || id != pipewire::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let pod = buffers_pod(node_w, node_h);
            let mut params = [Pod::from_bytes(&pod).expect("valid buffers pod")];
            if let Err(e) = for_params.update_params(&mut params) {
                tracing::warn!(error = %e, "could not state the screen node's buffer size");
            }
        })
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let picture = for_process.borrow();
            let written = match data.data() {
                Some(slice) => {
                    let n = slice.len().min(picture.len());
                    slice[..n].copy_from_slice(&picture[..n]);
                    n
                }
                None => 0,
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = (node_w * 4) as i32;
            *chunk.size_mut() = written as u32;
        })
        .register()
        .map_err(|e| format!("register stream listener: {e}"))?;

    let pod = format_pod(node_w, node_h);
    let mut params = [Pod::from_bytes(&pod).expect("valid format pod")];
    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("stream connect: {e}"))?;

    let mut fb = Framebuffer::new(node_w, node_h);
    let mut decoder: Option<VideoDecoder> = None;
    let mut announced = false;
    let loop_ref = main_loop.loop_();
    loop {
        loop_ref.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(4)));
        if !announced {
            let id = stream.node_id();
            if id != u32::MAX {
                announced = true;
                let _ = ready.send(Ok(id));
            }
        }
        let mut changed = false;
        let mut newest: Option<u32> = None;
        for _ in 0..16 {
            let command = match commands.try_recv() {
                Ok(c) => c,
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => return Ok(()),
            };
            match command {
                Command::Stop => return Ok(()),
                Command::Restart => decoder = None,
                Command::Resize { width, height } => fb.resize(width, height),
                Command::Grow { width, height } => fb.grow_to(width, height),
                Command::Frame { seq, dirty, raw } => match fb.blit(dirty, &raw) {
                    Ok(()) => {
                        changed = true;
                        newest = Some(newest.map_or(seq, |n| n.max(seq)));
                    }
                    Err(e) => tracing::warn!(error = %e, "dropping malformed tile"),
                },
                Command::VideoPacket { seq, packet } => {
                    if decoder.is_none() {
                        match VideoDecoder::open() {
                            Ok(d) => decoder = Some(d),
                            Err(e) => {
                                tracing::error!(error = %e, "no H.264 decoder; video frames dropped");
                                continue;
                            }
                        }
                    }
                    let Some(dec) = decoder.as_mut() else {
                        continue;
                    };
                    match dec.decode(&packet) {
                        Ok(pictures) => {
                            for pic in pictures {
                                if (fb.width, fb.height) != (pic.width, pic.height) {
                                    fb.resize(pic.width, pic.height);
                                }
                                let full = Rect {
                                    x: 0,
                                    y: 0,
                                    width: pic.width,
                                    height: pic.height,
                                };
                                if fb.blit(full, &pic.pixels).is_ok() {
                                    changed = true;
                                    newest = Some(newest.map_or(seq, |n| n.max(seq)));
                                }
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "video decode failed"),
                    }
                }
            }
        }
        if changed {
            scale_into(&fb, &mut outgoing.borrow_mut(), node_w, node_h);
            if let Err(e) = stream.trigger_process() {
                tracing::debug!(error = %e, "screen node could not push a frame");
            }
            if let Some(seq) = newest {
                let _ = presented.send(seq);
            }
        }
    }
}
