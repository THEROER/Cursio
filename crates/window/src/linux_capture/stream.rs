//! The PipeWire half: a stream on the portal's node, the format it accepts,
//! and the loop that turns each buffer into frames for the peer.

use crate::frame_pipeline::CapturePipeline;
use crate::linux_capture::portal::PortalCast;
use crate::linux_capture::replay::replay_input;
use crate::linux_capture::CaptureCommand;
use cursio_protocol::{WindowFrame, WindowId, WindowMeta};
use tokio::sync::mpsc;

/// State the PipeWire callbacks share.
///
/// `param_changed` learns the negotiated geometry, `process` needs it to
/// interpret the buffer, and both run on the PipeWire loop thread — hence
/// `Rc<RefCell<_>>` rather than any locking. Same shape as the audio backend's
/// per-stream `Cell`s, just with more to carry.
pub(super) struct StreamState {
    /// Negotiated video format; `None` until the first `param_changed`.
    format: Option<pipewire::spa::param::video::VideoInfoRaw>,
    /// Set once we've reported readiness, so it only happens on the first
    /// format.
    announced: bool,
    /// Lives here, on the PipeWire thread, rather than behind a lock: the
    /// encoder is driven from `process` and, between buffers, from the loop
    /// that iterates PipeWire — both this thread.
    pipeline: CapturePipeline<crate::video_encoder::FfmpegEncoders>,
}

/// Build the format Pod we offer PipeWire.
///
/// We ask for **BGRA only** — deliberately narrow. It is exactly the wire
/// format [`TileCodec::RawBgra8888`] specifies, so accepting anything else
/// would mean a per-frame colour conversion on the hot path. The compositor can
/// produce BGRA, so constraining here costs nothing and saves the conversion.
/// Size and framerate stay ranges: the portal decides the window's real size,
/// and the compositor caps the rate.
pub(super) fn format_param_pod() -> Vec<u8> {
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
            VideoFormat::BGRA,
            VideoFormat::BGRA,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 640,
                height: 480
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 144, denom: 1 }
        ),
    );

    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serializing a static format pod cannot fail")
        .0
        .into_inner()
}

/// Attach a PipeWire stream to the portal's node and pump frames until stopped.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_pipewire_loop(
    id: WindowId,
    title: String,
    cast: PortalCast,
    frames_tx: mpsc::Sender<WindowFrame>,
    mut cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
    ready_tx: &std::sync::mpsc::Sender<Result<WindowMeta, String>>,
    rt: &tokio::runtime::Runtime,
    quality: std::sync::Arc<crate::frame_pipeline::AdaptiveQuality>,
    last_seq: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> Result<(), String> {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use pipewire::{
        context::ContextRc,
        main_loop::MainLoopRc,
        properties::properties,
        spa::{pod::Pod, utils::Direction},
        stream::{StreamFlags, StreamRc},
    };

    pipewire::init();

    // Split the cast up front: the fd is consumed by `connect_fd_rc`, while the
    // portal proxy/session must stay alive for the whole loop (they are both
    // what keeps the cast open and how input is replayed).
    let PortalCast {
        node_id,
        fd,
        input,
        _view_session,
        input_granted,
    } = cast;

    let main_loop = MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context = ContextRc::new(&main_loop, None).map_err(|e| format!("context: {e}"))?;
    // Connect through the portal's fd, not the session bus: this is the
    // permission-scoped PipeWire the portal opened for us.
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| format!("connect to portal pipewire: {e}"))?;

    let stream = StreamRc::new(
        core.clone(),
        "cursio-window-capture",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("stream: {e}"))?;

    let state = Rc::new(RefCell::new(StreamState {
        format: None,
        announced: false,
        pipeline: CapturePipeline::new(id, Default::default(), quality, last_seq, frames_tx),
    }));

    let ready = ready_tx.clone();
    let state_fmt = state.clone();
    let state_proc = state.clone();

    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_, (), param_id, param| {
            let Some(param) = param else { return };
            if param_id != pipewire::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) =
                pipewire::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pipewire::spa::param::format::MediaType::Video
                || media_subtype != pipewire::spa::param::format::MediaSubtype::Raw
            {
                return;
            }

            let mut info = pipewire::spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                tracing::warn!("could not parse negotiated video format");
                return;
            }
            let size = info.size();
            tracing::info!(
                width = size.width,
                height = size.height,
                format = ?info.format(),
                "window capture format negotiated"
            );

            let mut st = state_fmt.borrow_mut();
            st.format = Some(info);
            // The first negotiated format is what makes the offer meaningful —
            // only now do we know the real geometry to tell the peer about.
            if !st.announced {
                st.announced = true;
                let _ = ready.send(Ok(WindowMeta {
                    id,
                    title: title.clone(),
                    width: size.width,
                    height: size.height,
                }));
            }
        })
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };

            let chunk = data.chunk();
            let (offset, size, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride() as usize,
            );
            if size == 0 || stride == 0 {
                return;
            }
            // Mutter queues a buffer marked corrupted when it had no new
            // picture to record into it (a cursor-only update, a failed
            // blit). Its memory is whatever an earlier frame left in that pool
            // slot, and diffing it as if it were current sends the viewer a
            // picture from several frames ago.
            if chunk
                .flags()
                .contains(pipewire::spa::buffer::ChunkFlags::CORRUPTED)
            {
                return;
            }
            let Some(bytes) = data.data() else { return };

            let mut st = state_proc.borrow_mut();
            let Some(info) = st.format else { return };
            let (width, height) = (info.size().width, info.size().height);
            let row_bytes = (width as usize) * 4;

            // PipeWire rows are `stride` bytes apart, which may exceed the
            // visible `width * 4` (alignment padding). Repack tightly so every
            // later stage — differ, tile extraction, the wire format — can
            // assume no padding.
            let mut frame = Vec::with_capacity(row_bytes * height as usize);
            for y in 0..height as usize {
                let start = offset + y * stride;
                let end = start + row_bytes;
                if end > bytes.len() {
                    // Short buffer: the format and the data disagree, so this
                    // frame isn't safe to interpret. Skip rather than send
                    // garbage or panic on the RT thread.
                    return;
                }
                frame.extend_from_slice(&bytes[start..end]);
            }

            st.pipeline
                .frame(&frame, width, height, std::time::Instant::now());
        })
        .register()
        .map_err(|e| format!("register stream listener: {e}"))?;

    let pod_bytes = format_param_pod();
    let mut params = [Pod::from_bytes(&pod_bytes).expect("valid format pod")];
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("stream connect: {e}"))?;

    tracing::info!(node_id, "window capture stream connected");

    // Drive the loop, servicing commands between iterations — PipeWire's loop
    // has no cross-thread "run until signalled" (same approach as the audio
    // capture loop).
    let mut warned_no_input = false;
    'outer: loop {
        // Drain queued input before iterating: replay should track the user's
        // hand, and a batch that sat through a 50 ms PipeWire tick would feel
        // like lag. Bounded so a flood of motion can't stall capture entirely.
        for _ in 0..64 {
            match cmd_rx.try_recv() {
                Ok(CaptureCommand::Input(event)) => {
                    let Some((remote, session)) = input.as_ref() else {
                        continue;
                    };
                    if !input_granted {
                        if !warned_no_input {
                            warned_no_input = true;
                            tracing::warn!(
                                "ignoring viewer input: the portal granted no input devices"
                            );
                        }
                        continue;
                    }
                    // `block_on` is safe here: this thread's runtime is
                    // current-thread and nothing else is driving it while the
                    // PipeWire loop is between iterations.
                    if let Err(e) = rt.block_on(replay_input(remote, session, node_id, event)) {
                        tracing::debug!(error = %e, "could not replay input onto the window");
                    }
                }
                Ok(CaptureCommand::Keyframe) => state.borrow_mut().pipeline.request_resync(),
                Ok(CaptureCommand::Stop) => break 'outer,
                Err(mpsc::error::TryRecvError::Disconnected) => break 'outer,
                Err(mpsc::error::TryRecvError::Empty) => break,
            }
        }
        main_loop
            .loop_()
            .iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(16)));
        // Between buffers, on this same thread: a still screen delivers none,
        // and what the viewer is owed must not wait for one.
        state.borrow_mut().pipeline.idle(std::time::Instant::now());
        if state.borrow().pipeline.closed() {
            break;
        }
    }

    tracing::info!("window capture stream stopped");
    Ok(())
}
