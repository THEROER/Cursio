//! Remote-screen views: an app or a monitor of a peer, offered to this
//! machine's screen-sharing software without being drawn on its desktop.
//!
//! Discord, OBS or a browser call can only offer what this machine has. What
//! "has" means differs, so the view does too:
//!
//! - **Windows**: those apps list windows, so the view is one — titled
//!   `Cursio — <peer>`, one per peer, kept for the life of the link, but
//!   never on screen (see the Windows surrogate's `unseen` module for the
//!   measurements behind "listed and recorded, yet unseen"). Its source is
//!   chosen from a notification-area icon.
//! - **Linux (GNOME)**: those apps take pixels only from the ScreenCast
//!   portal, so the view is a PipeWire node, opened when a capture app picks
//!   the peer's source in Cursio's portal backend dialog and closed when it is
//!   done. Nothing is kept open per peer; the request travels through
//!   [`ScreenHub`], because the portal runs apart from the session loops
//!   ([`ScreenViewer::on_demand`]).
//!
//! The first version drew a visible window per peer on the viewing desktop,
//! purely so it could be captured — rendering for nobody, which is what the
//! user asked not to have.
//!
//! ## Pulled by the viewer, unlike a transfer
//!
//! A window transfer is pushed: dragging it across the edge is the host's user
//! saying "show this over there". Here the request comes from the viewer. In a
//! mesh of strangers that would be a spyware primitive; in Cursio's it is not a
//! new grant. Every peer is one of the user's own machines, admitted by a
//! password typed on both, and each of them can already drive the other's
//! keyboard and mouse and read its disks — watching its screen is strictly
//! less. Hosting still stays a choice: a node started with `--no-screen-host`
//! answers every request with [`WindowMsg::SourcesRefused`] and a reason, so
//! the viewer can say why nothing appears instead of waiting on silence.
//! Viewing stays passive in the other sense — nothing here opens unless the
//! user asked for views with `--remote-screen`.
//!
//! ## One window across every switch (Windows)
//!
//! Screen-sharing software captures a *window*; recreate the window and the
//! share ends. So a source switch never closes the view. (On Linux a switch
//! is a new pick in the capture app, which asks the portal afresh, so each
//! pick gets its own node; one per peer at a time, a newer pick taking the
//! stream from an older one.) The host swaps the
//! capture underneath one reserved stream id ([`SCREEN_STREAM_ID`]) and sends
//! [`WindowMsg::SourceShown`] ahead of the new source's first frame; the view
//! restarts its decoder and picture on that marker and letterboxes whatever
//! size arrives. The old capture keeps streaming until the new one is actually
//! up, because on GNOME "up" can mean a portal picker waiting on the host.
//!
//! The host's half is [`ScreenHost`], the viewer's [`ScreenViewer`]; the window
//! module owns one of each and hands them the messages that are theirs.

mod host;
mod viewer;

pub use host::ScreenHost;
pub use viewer::ScreenViewer;

/// The view's window title, which is what a screen-sharing picker lists.
///
/// Fixed per peer, never the current source: the application lists windows by
/// title, and one that renamed itself on every switch would be hard to find
/// again in that list.
pub fn view_title(peer_name: &str) -> String {
    format!("Cursio — {peer_name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen_hub::ScreenHub;
    use crate::test_support::RecordingTx;
    use crate::window_session::KeyframeRequests;
    use cursio_protocol::Message;
    use cursio_protocol::ScreenSource;
    use cursio_protocol::SourceId;
    use cursio_protocol::SourceKind;
    use cursio_protocol::WindowFrame;
    use cursio_protocol::WindowMsg;
    use cursio_protocol::SCREEN_STREAM_ID;
    use cursio_protocol::{ModuleId, Rect, TileCodec};
    use cursio_window::window_test_support::{FakeRenderer, FakeScreenSources, FakeSurrogate};
    use cursio_window::CaptureSession;
    use cursio_window::ScreenSources;
    use cursio_window::SourcePick;
    use cursio_window::WindowError;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    fn source(id: SourceId, kind: SourceKind, title: &str, picked_on_host: bool) -> ScreenSource {
        ScreenSource {
            id,
            kind,
            title: title.into(),
            width: 800,
            height: 600,
            picked_on_host,
        }
    }

    fn lightmoon() -> FakeScreenSources {
        FakeScreenSources::new(vec![
            source(1, SourceKind::Window, "Firefox", false),
            source(0x8000_0000, SourceKind::Monitor, "Screen 1", false),
            source(2, SourceKind::Window, "Terminal", false),
        ])
    }

    fn frame(seq: u32) -> WindowFrame {
        WindowFrame {
            id: SCREEN_STREAM_ID,
            seq,
            dirty: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            codec: TileCodec::RawBgra8888,
            data: vec![0; 4],
        }
    }

    /// A host and a viewer joined back to back, standing in for two machines.
    struct Pair {
        host: ScreenHost,
        viewer: ScreenViewer<FakeSurrogate>,
        renderer: FakeRenderer,
    }

    impl Pair {
        fn new(sources: &FakeScreenSources) -> Self {
            Self {
                host: ScreenHost::serving(Arc::new(sources.clone())),
                viewer: ScreenViewer::new("Lightmoon".into()),
                renderer: FakeRenderer::default(),
            }
        }

        /// One round trip: the viewer pumps, the host takes what it sent and
        /// pumps, the viewer takes the host's answer. Returns the host's traffic.
        async fn exchange(&mut self) -> RecordingTx {
            let mut up = RecordingTx::default();
            self.viewer.pump(&mut up).await.unwrap();
            for msg in up.decoded::<WindowMsg>(ModuleId::WINDOW) {
                self.host.handle(&msg);
            }
            let mut down = RecordingTx::default();
            self.host.pump(&mut down).await.unwrap();
            for msg in &down.sent {
                match msg {
                    Message::WindowFrame(f) => self.viewer.apply_frame(f),
                    other => {
                        if let Some(control) =
                            other.decode_module::<WindowMsg>(ModuleId::WINDOW).unwrap()
                        {
                            self.viewer.handle(&control, &mut self.renderer);
                        }
                    }
                }
            }
            down
        }

        /// Exchange until the host has nothing listing or opening, since both
        /// run on threads of their own.
        async fn settle(&mut self) -> Vec<WindowMsg> {
            let mut seen = Vec::new();
            for _ in 0..400 {
                seen.extend(self.exchange().await.decoded::<WindowMsg>(ModuleId::WINDOW));
                if host_idle(&self.host) && self.viewer.outbox.is_empty() {
                    let extra = self.exchange().await.decoded::<WindowMsg>(ModuleId::WINDOW);
                    seen.extend(extra);
                    if host_idle(&self.host) {
                        return seen;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the host never finished listing or opening a source");
        }
    }

    fn host_idle(host: &ScreenHost) -> bool {
        host.opening.is_none() && host.listing.is_none()
    }

    /// Pump a lone host until its listing and opening threads have answered.
    async fn pump_settled(host: &mut ScreenHost, tx: &mut RecordingTx) {
        for _ in 0..400 {
            host.pump(tx).await.unwrap();
            if host_idle(host) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the host never finished listing or opening a source");
    }

    #[tokio::test]
    async fn the_view_opens_on_the_first_list_and_shows_the_first_monitor() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);

        pair.settle().await;

        assert_eq!(pair.renderer.views_opened, vec!["Cursio — Lightmoon"]);
        assert_eq!(pair.host.showing(), Some(0x8000_0000));
        let view = pair.viewer.view().unwrap();
        assert_eq!(view.sources.len(), 3);
        assert_eq!(view.restarts, vec!["Screen 1"]);
    }

    #[tokio::test]
    async fn the_list_is_resent_only_when_it_changes() {
        let sources = lightmoon();
        let mut host = ScreenHost::serving(Arc::new(sources.clone()));
        host.handle(&WindowMsg::ListSources);

        let mut tx = RecordingTx::default();
        pump_settled(&mut host, &mut tx).await;
        host.next_poll = Instant::now();
        pump_settled(&mut host, &mut tx).await;
        let lists = |tx: &RecordingTx| {
            tx.decoded::<WindowMsg>(ModuleId::WINDOW)
                .iter()
                .filter(|m| matches!(m, WindowMsg::Sources(_)))
                .count()
        };
        assert_eq!(lists(&tx), 1, "an unchanged list must not be resent");

        sources.set_sources(vec![source(9, SourceKind::Window, "Discord", false)]);
        host.next_poll = Instant::now();
        pump_settled(&mut host, &mut tx).await;
        assert_eq!(lists(&tx), 2, "a window opening must reach the viewer");
    }

    /// Enumerating sources never holds up the loop the cursor rides on.
    ///
    /// On Windows a window's title is answered by the thread that owns the
    /// window, and a view's own window thread is busy decoding: listing on the
    /// session loop stalled it 50–220 ms every two seconds while a view ran.
    #[tokio::test]
    async fn a_slow_source_list_does_not_hold_up_the_pump() {
        let sources = lightmoon();
        let mut host = ScreenHost::serving(Arc::new(sources.clone()));
        host.handle(&WindowMsg::ListSources);
        sources.set_list_stalled(true);

        let mut tx = RecordingTx::default();
        let started = std::time::Instant::now();
        for _ in 0..3 {
            host.pump(&mut tx).await.unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "the pump waited on the enumeration"
        );
        assert!(tx.decoded::<WindowMsg>(ModuleId::WINDOW).is_empty());

        sources.set_list_stalled(false);
        pump_settled(&mut host, &mut tx).await;
        assert!(matches!(
            tx.decoded::<WindowMsg>(ModuleId::WINDOW).as_slice(),
            [WindowMsg::Sources(list)] if list.len() == 3
        ));
    }

    #[tokio::test]
    async fn a_host_that_opted_out_refuses_with_a_reason_and_no_view_opens() {
        let mut host = ScreenHost::refusing("started with --no-screen-host");
        let mut viewer: ScreenViewer<FakeSurrogate> = ScreenViewer::new("Nocturne".into());
        let mut renderer = FakeRenderer::default();

        let mut up = RecordingTx::default();
        viewer.pump(&mut up).await.unwrap();
        for msg in up.decoded::<WindowMsg>(ModuleId::WINDOW) {
            host.handle(&msg);
        }
        let mut down = RecordingTx::default();
        host.pump(&mut down).await.unwrap();
        let answers = down.decoded::<WindowMsg>(ModuleId::WINDOW);
        assert!(matches!(
            answers.as_slice(),
            [WindowMsg::SourcesRefused { reason }] if reason.contains("--no-screen-host")
        ));
        for msg in &answers {
            viewer.handle(msg, &mut renderer);
        }
        assert!(renderer.views_opened.is_empty());
    }

    #[tokio::test]
    async fn switching_source_keeps_the_view_and_restarts_its_stream() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        let monitor_frames = sources.frames_for(0x8000_0000).unwrap();

        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Source(2));
        let traffic = pair.settle().await;

        assert_eq!(pair.host.showing(), Some(2));
        assert_eq!(
            pair.renderer.views_opened.len(),
            1,
            "a switch must never recreate the window a screen share is capturing"
        );
        assert_eq!(
            pair.viewer.view().unwrap().restarts,
            vec!["Screen 1", "Terminal"]
        );
        assert!(traffic
            .iter()
            .any(|m| matches!(m, WindowMsg::SourceShown { source: 2, .. })));
        assert!(
            monitor_frames.is_closed(),
            "the previous source's capture must stop once the new one is up"
        );
    }

    #[tokio::test]
    async fn frames_after_a_switch_reach_the_same_view() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        sources
            .frames_for(0x8000_0000)
            .unwrap()
            .try_send(frame(1))
            .unwrap();
        pair.exchange().await;

        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Source(1));
        pair.settle().await;
        sources.frames_for(1).unwrap().try_send(frame(1)).unwrap();
        pair.exchange().await;

        assert_eq!(pair.viewer.view().unwrap().applied.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_open_says_why_and_keeps_showing_the_old_source() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        sources.fail_next("the user cancelled the dialog");
        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Source(1));
        let traffic = pair.settle().await;

        assert!(traffic.iter().any(|m| matches!(
            m,
            WindowMsg::SourceEnded { reason } if reason.contains("cancelled")
        )));
        assert_eq!(pair.host.showing(), Some(0x8000_0000));
    }

    /// Opens through a task of its own, as a phone's screen does.
    struct SpawningSources(FakeScreenSources);

    impl ScreenSources for SpawningSources {
        fn list(&self) -> Result<Vec<ScreenSource>, WindowError> {
            self.0.list()
        }

        fn open(
            &self,
            id: cursio_protocol::WindowId,
            source: &ScreenSource,
        ) -> Result<Box<dyn CaptureSession + Send>, WindowError> {
            tokio::spawn(async {});
            self.0.open(id, source)
        }
    }

    #[tokio::test]
    async fn a_source_that_spawns_a_task_while_opening_is_shown() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.host = ScreenHost::serving(Arc::new(SpawningSources(sources.clone())));

        let traffic = pair.settle().await;

        assert!(
            !traffic
                .iter()
                .any(|m| matches!(m, WindowMsg::SourceEnded { .. })),
            "{traffic:?}"
        );
        assert_eq!(pair.host.showing(), Some(0x8000_0000));
    }

    #[tokio::test]
    async fn stopping_ends_the_stream_but_keeps_the_view() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        pair.viewer
            .view_mut()
            .unwrap()
            .picks
            .push_back(SourcePick::Stop);
        pair.settle().await;

        assert_eq!(pair.host.showing(), None);
        let view = pair.viewer.view().unwrap();
        assert!(view.closed.is_none());
        assert_eq!(view.restarts.last().map(String::as_str), Some(""));
    }

    #[tokio::test]
    async fn closing_the_view_stops_the_host_and_it_stays_closed() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        pair.viewer.view_mut().unwrap().user_closes();
        pair.settle().await;
        pair.host.next_poll = Instant::now();
        sources.set_sources(vec![source(5, SourceKind::Window, "New", false)]);
        pair.settle().await;

        assert_eq!(pair.host.showing(), None);
        assert!(pair.viewer.view().is_none());
        assert_eq!(
            pair.renderer.views_opened.len(),
            1,
            "a list refresh must not reopen a view the user closed"
        );
    }

    #[tokio::test]
    async fn stepping_skips_sources_that_open_a_picker_on_the_host() {
        let sources = FakeScreenSources::new(vec![
            source(1, SourceKind::Window, "Choose a window…", true),
            source(0x8000_0000, SourceKind::Monitor, "Screen 1", false),
            source(0x8000_0001, SourceKind::Monitor, "Screen 2", false),
        ]);
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        for _ in 0..2 {
            pair.viewer
                .view_mut()
                .unwrap()
                .picks
                .push_back(SourcePick::Next);
            pair.settle().await;
        }

        assert_eq!(
            sources.opened(),
            vec![0x8000_0000, 0x8000_0001, 0x8000_0000]
        );
    }

    #[tokio::test]
    async fn a_host_offering_only_pickers_shows_nothing_until_asked() {
        let sources = FakeScreenSources::new(vec![
            source(1, SourceKind::Window, "Choose a window…", true),
            source(2, SourceKind::Monitor, "Choose a screen…", true),
        ]);
        let mut pair = Pair::new(&sources);
        pair.settle().await;

        assert_eq!(pair.renderer.views_opened.len(), 1);
        assert!(
            sources.opened().is_empty(),
            "a dialog must not pop up on the other machine unasked"
        );
    }

    #[tokio::test]
    async fn a_source_gone_since_the_list_was_sent_is_refused_by_name() {
        let sources = lightmoon();
        let mut host = ScreenHost::serving(Arc::new(sources.clone()));
        sources.set_sources(Vec::new());

        host.handle(&WindowMsg::ShowSource { source: 1 });
        let mut tx = RecordingTx::default();
        pump_settled(&mut host, &mut tx).await;

        assert!(matches!(
            tx.decoded::<WindowMsg>(ModuleId::WINDOW).as_slice(),
            [WindowMsg::SourceEnded { reason }] if reason.contains("no longer exists")
        ));
    }

    fn on_demand(sources: &FakeScreenSources) -> (Pair, ScreenHub) {
        let hub = ScreenHub::default();
        let mut pair = Pair::new(sources);
        pair.viewer = ScreenViewer::new("Lightmoon".into()).on_demand(hub.clone(), "lm".into());
        (pair, hub)
    }

    #[tokio::test]
    async fn on_demand_the_peers_sources_reach_the_hub_and_nothing_opens_unasked() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);

        pair.settle().await;

        assert_eq!(hub.peers().len(), 1);
        assert_eq!(hub.peers()[0].2.len(), 3);
        assert!(pair.renderer.views_opened.is_empty());
        assert!(
            sources.opened().is_empty(),
            "no capture may start until a capture app asks"
        );
    }

    #[tokio::test]
    async fn a_capture_apps_request_publishes_a_node_once_the_host_shows_the_source() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;

        let answer = hub.show("lm", 2);
        pair.settle().await;

        let shown = answer.await.unwrap().expect("the source is shown");
        assert_eq!(shown.node_id, 100);
        assert_eq!((shown.width, shown.height), (800, 600));
        assert_eq!(shown.kind, SourceKind::Window);
        assert_eq!(pair.host.showing(), Some(2));
        assert_eq!(
            pair.renderer.views_opened,
            ["Cursio — Lightmoon · Terminal"]
        );
    }

    #[tokio::test]
    async fn the_capture_app_stopping_ends_the_node_and_the_hosts_capture() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;
        let shown = {
            let answer = hub.show("lm", 1);
            pair.settle().await;
            answer.await.unwrap().unwrap()
        };

        hub.stop("lm", shown.token);
        pair.settle().await;

        assert_eq!(pair.host.showing(), None);
        assert!(pair.viewer.view().is_none());
    }

    #[tokio::test]
    async fn a_newer_request_takes_the_stream_and_the_old_holder_is_told() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;
        let first = {
            let answer = hub.show("lm", 1);
            pair.settle().await;
            answer.await.unwrap().unwrap()
        };

        let second = hub.show("lm", 0x8000_0000);
        pair.settle().await;

        assert!(
            first.ended.await.is_ok(),
            "the first app must learn its share ended"
        );
        let second = second.await.unwrap().unwrap();
        assert_eq!(second.kind, SourceKind::Monitor);
        assert_eq!(pair.host.showing(), Some(0x8000_0000));
    }

    #[tokio::test]
    async fn a_source_the_host_cannot_open_fails_the_request_with_its_reason() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;

        sources.fail_next("the user cancelled the dialog");
        let answer = hub.show("lm", 1);
        pair.settle().await;

        let err = answer.await.unwrap().unwrap_err();
        assert!(err.contains("cancelled"));
        assert!(pair.renderer.views_opened.is_empty());
    }

    #[tokio::test]
    async fn the_link_ending_withdraws_the_peer_and_ends_its_share() {
        let sources = lightmoon();
        let (mut pair, hub) = on_demand(&sources);
        pair.settle().await;
        let shown = {
            let answer = hub.show("lm", 1);
            pair.settle().await;
            answer.await.unwrap().unwrap()
        };

        pair.viewer.shutdown();

        assert!(shown.ended.await.is_ok());
        assert!(hub.peers().is_empty());
    }

    #[tokio::test]
    async fn the_link_ending_closes_the_view_and_stops_the_capture() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        let frames = sources.frames_for(0x8000_0000).unwrap();

        pair.host.shutdown();
        pair.viewer.shutdown();

        assert!(frames.is_closed());
        assert!(pair.viewer.view().is_none());
    }

    #[tokio::test]
    async fn a_shown_source_asks_its_capture_for_a_whole_picture() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        pair.exchange().await;
        // Asked on `SourceShown`, before any frame could say whether the first
        // one arrived whole.
        assert_eq!(sources.keyframe_requests(), 1);
    }

    #[tokio::test]
    async fn a_view_stuck_without_a_keyframe_asks_at_most_once_a_second() {
        let sources = lightmoon();
        let mut pair = Pair::new(&sources);
        pair.settle().await;
        pair.exchange().await;
        let before = sources.keyframe_requests();

        // The decoder raises its want for every packet it drops.
        for _ in 0..20 {
            pair.viewer.view_mut().unwrap().wants_keyframe = true;
            pair.exchange().await;
        }
        assert_eq!(
            sources.keyframe_requests(),
            before,
            "the ask on SourceShown was just now; the next waits its second"
        );
        assert!(
            pair.viewer.view_mut().unwrap().wants_keyframe,
            "a want raised while waiting is kept for when a request may go"
        );

        pair.viewer.keyframe_asks = KeyframeRequests::default();
        pair.exchange().await;
        assert_eq!(sources.keyframe_requests(), before + 1);
    }
}
