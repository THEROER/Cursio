//! A live-test window's log lines, as both halves' tests read them.

pub(super) const WINDOW: &str = "\
2026-09-27T09:52:24.083471Z  INFO cursio_core::session: input latency path=\"received->injected\" events=2154 p50_us=33 p99_us=271 max_us=347
2026-09-27T09:47:21.071981Z DEBUG cursio_transport::priority: send queue delay input_frames=1084 input_p50=20µs input_p99=48µs input_max=221.006µs video_p99=102.107486ms bulk_p99=0ns
2026-09-27T09:57:14.070526Z  WARN cursio_core::session: session loop turn was slow; input and cursor handoffs waited took_ms=4986
2026-09-27T09:18:02.450427Z  WARN cursio_core::session: module pump blocked the session loop module=\"audio\" took_ms=118
2026-09-27T09:56:08.282860Z  WARN cursio_core::module_host: module pump was slow; only this module's traffic waited module=\"usb\" took_ms=82
2026-09-27T09:43:09.637638Z  WARN cursio: peer session ended with error error=connection closed by peer
2026-09-27T09:56:20.376756Z  INFO cursio_core::audio_session: audio health sent=59 played=4 silent_dropped=1 unwanted_dropped=1 over_budget=0 stale_dropped=2 streams=11
2026-09-27T09:56:50.404795Z  INFO cursio_core::audio_session: audio health sent=59 played=10 silent_dropped=1 unwanted_dropped=1 over_budget=0 stale_dropped=5 streams=11
2026-09-27T09:52:49.435713Z  INFO cursio_core::files_session: file sharing health peer=Lightmoon read_kb_s=0 rtt_ms=3 rtt_max_ms=41 ops=3 failed=1 in_flight=0 by_kind= served=stat=41 served_slow=2
2026-09-27T08:33:30.535091Z  WARN cursio_core::files_session: a request from the peer took long enough that its caller may have given up kind=\"read\" took_ms=2022
2026-09-27T09:00:00.000000Z ERROR cursio_audio::pipewire_backend: failed to link app to capture after retries app_id=7
";
