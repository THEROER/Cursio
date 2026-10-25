//! The link's timer: acknowledgements, keepalives, silent and degraded paths,
//! and the hold running out.

use crate::crypto::link_tag;
use crate::lane::Lane;
use crate::resume::frame::ACK_ALL_PATHS;
use crate::resume::inner::Inner;
use crate::resume::ACK_EVERY;
use crate::resume::ACK_SOON;
use crate::resume::ACTIVE_SILENCE;
use crate::resume::DEGRADED_FOR;
use crate::resume::HOLD;
use crate::resume::STALL;
use crate::resume::STANDBY_KEEPALIVE;
use crate::resume::STANDBY_SILENCE;
use crate::resume::TICK;
use std::sync::Weak;
use std::time::Duration;
use tokio::time::Instant;

/// Watches the paths: keepalives and acknowledgements, silence, stalls, and
/// the hold running out.
pub(super) async fn supervise(weak: Weak<Inner>) {
    let mut tick = tokio::time::interval(TICK);
    let mut last_ack = Instant::now();
    let mut last_keepalive = Instant::now();
    loop {
        tick.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        let now = Instant::now();
        let mut silent: Vec<(u64, Duration)> = Vec::new();
        let mut degraded = None;
        let mut end = None;
        let ack_due;
        {
            let mut state = inner.lock();
            if state.ended.is_some() {
                return;
            }
            let carrying: Vec<u64> = Lane::ALL
                .into_iter()
                .filter_map(|lane| state.carrying(lane, now).map(|p| p.id))
                .collect();
            for path in &state.paths {
                let limit = if carrying.contains(&path.id) {
                    ACTIVE_SILENCE
                } else {
                    STANDBY_SILENCE
                };
                let quiet = now.duration_since(path.last_heard);
                if quiet > limit {
                    silent.push((path.id, quiet));
                }
            }
            for id in carrying {
                let stalled = state.paths.len() > 1
                    && state.lanes.iter().flat_map(|d| d.unacked.iter()).any(|k| {
                        k.sent_on == Some(id)
                            && k.sent_at.is_some_and(|at| now.duration_since(at) > STALL)
                    });
                if stalled {
                    if let Some(path) = state.paths.iter_mut().find(|p| p.id == id) {
                        path.degraded_until = Some(now + DEGRADED_FOR);
                        degraded = Some(id);
                    }
                }
            }
            if state.carrying(Lane::Interactive, now).is_none()
                && state
                    .pathless_since
                    .is_some_and(|since| now.duration_since(since) > HOLD)
            {
                end = Some(format!(
                    "no connection came back within {} s",
                    HOLD.as_secs()
                ));
            }
            let interval = if state
                .bulk_active
                .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
            {
                Duration::from_millis(200)
            } else {
                ACK_EVERY
            };
            let probe_path = state.carrying(Lane::Interactive, now).map(|p| p.id);
            state.next_probe += 1;
            let nonce = state.next_probe;
            if let Some(path) = state.paths.iter_mut().find(|p| Some(p.id) == probe_path) {
                if let Some((_, at)) = path.probe {
                    if interval < ACK_EVERY && now.duration_since(at) > interval {
                        inner
                            .congestion
                            .lock()
                            .unwrap()
                            .observe(now.duration_since(at), now, true);
                    }
                }
                if now.duration_since(path.last_probe) >= interval
                    && path
                        .probe
                        .is_none_or(|(_, at)| now.duration_since(at) > STALL)
                {
                    let mut ping = vec![link_tag::PING];
                    ping.extend_from_slice(&nonce.to_be_bytes());
                    if path.writer.try_send(ping).is_ok() {
                        path.probe = Some((nonce, now));
                        path.last_probe = now;
                    }
                }
            }
            ack_due = state.ack_due;
        }

        for (id, quiet) in silent {
            inner.path_failed(id, &format!("silent for {} ms", quiet.as_millis()));
        }
        if let Some(id) = degraded {
            tracing::info!(
                path = id,
                "the carrying path stopped delivering; moving the session to another"
            );
            inner.kick(0);
        }
        if let Some(why) = end {
            inner.end(why);
            return;
        }
        if now.duration_since(last_keepalive) >= STANDBY_KEEPALIVE {
            last_keepalive = now;
            last_ack = now;
            inner.kick(ACK_ALL_PATHS);
        } else if (ack_due && now.duration_since(last_ack) >= ACK_SOON)
            || now.duration_since(last_ack) >= ACK_EVERY
        {
            last_ack = now;
            inner.kick(0);
        }
    }
}
