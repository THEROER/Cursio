//! The link's timer: acknowledgements, keepalives, silent and degraded paths,
//! and the hold running out.

use crate::seal::link_tag;
use crate::session::frame::ACK_ALL_PATHS;
use crate::session::inner::Inner;
use crate::session::PathKind;
use crate::session::ACK_EVERY;
use crate::session::ACK_SOON;
use crate::session::ACTIVE_SILENCE;
use crate::session::DEGRADED_FOR;
use crate::session::HOLD;
use crate::session::NETWORK_GIVE_UP;
use crate::session::STALL;
use crate::session::STANDBY_CARRYING_SILENCE;
use crate::session::STANDBY_KEEPALIVE;
use crate::session::STANDBY_SILENCE;
use crate::session::TICK;
use crate::LaneId;
use std::sync::Weak;
use std::time::Duration;
use tokio::time::Instant;

/// Watches the paths: keepalives and acknowledgements, silence, stalls, and
/// the hold running out.
pub(super) async fn supervise(weak: Weak<Inner>) {
    let mut tick = tokio::time::interval(TICK);
    let mut last_ack = Instant::now();
    let mut last_keepalive = Instant::now();
    let mut last_check = Instant::now();
    loop {
        tick.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        let now = Instant::now();
        if let Some(binding) = &inner.binding {
            let wall = std::time::SystemTime::now();
            if binding.past_deadline(wall) || now.duration_since(last_check) >= binding.check_every
            {
                last_check = now;
                if !inner.verify_peer() {
                    return;
                }
            }
        }
        let mut silent: Vec<(u64, Duration)> = Vec::new();
        let mut degraded = None;
        let mut end = None;
        let mut overdue = None;
        let ack_due;
        {
            let mut state = inner.lock();
            if state.ended.is_some() {
                return;
            }
            let carrying: Vec<u64> = inner
                .classes
                .lane_ids()
                .filter_map(|lane| state.carrying(lane, now).map(|p| p.id))
                .collect();
            let mut quiet_network: Vec<u64> = Vec::new();
            for path in &state.paths {
                let quiet = now.duration_since(path.last_heard);
                let network = path.kind != PathKind::Standby;
                if network && path.suspect_since.is_some() {
                    if quiet > NETWORK_GIVE_UP {
                        silent.push((path.id, quiet));
                    }
                    continue;
                }
                let limit = if !carrying.contains(&path.id) {
                    STANDBY_SILENCE
                } else if network {
                    ACTIVE_SILENCE
                } else {
                    STANDBY_CARRYING_SILENCE
                };
                if quiet > limit {
                    if network && limit == ACTIVE_SILENCE {
                        quiet_network.push(path.id);
                    } else {
                        silent.push((path.id, quiet));
                    }
                }
            }
            // A carrying network path gone quiet is kept while its
            // replacement is dialled, rather than closed: passed over for
            // carrying, and missing as far as the mesh's dialling is
            // concerned, so a standby takes the session and a new
            // connection is placed at once. A blip shorter than that leaves
            // the old connection to carry on as if nothing happened; a
            // replacement that joins first replaces it.
            for id in quiet_network {
                if let Some(path) = state.paths.iter_mut().find(|p| p.id == id) {
                    path.suspect_since = Some(now);
                    path.degraded_until = Some(now + NETWORK_GIVE_UP);
                    path.probe = None;
                    tracing::info!(
                        session = %inner.label(),
                        path = id,
                        kind = inner.kind_name(path.kind),
                        "a path went quiet; keeping it while another is placed"
                    );
                    degraded = Some(id);
                }
            }
            let interactive = state.carrying(LaneId::INTERACTIVE, now).map(|p| p.id);
            for id in carrying {
                let stalled = state.paths.len() > 1
                    && state.lanes.iter().flat_map(|d| d.unacked.iter()).any(|k| {
                        k.sent_on == Some(id)
                            && k.sent_at.is_some_and(|at| now.duration_since(at) > STALL)
                    });
                let stalled =
                    stalled || (Some(id) == interactive && state.input_stalled_on(id, now));
                if stalled {
                    if let Some(path) = state.paths.iter_mut().find(|p| p.id == id) {
                        path.degraded_until = Some(now + DEGRADED_FOR);
                        // A ping lost in the stall would otherwise still be
                        // outstanding when the hold ends, and degrade the path
                        // again at once however well it carries by then.
                        path.probe = None;
                        degraded = Some(id);
                    }
                }
            }
            if state.carrying(LaneId::INTERACTIVE, now).is_none()
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
            let probe_path = state.carrying(LaneId::INTERACTIVE, now).map(|p| p.id);
            state.next_probe += 1;
            let nonce = state.next_probe;
            if let Some(path) = state.paths.iter_mut().find(|p| Some(p.id) == probe_path) {
                if let Some((_, at)) = path.probe {
                    if interval < ACK_EVERY && now.duration_since(at) > interval {
                        overdue = Some((path.id, now.duration_since(at)));
                    }
                }
                if now.duration_since(path.last_probe) >= interval
                    && path
                        .probe
                        .is_none_or(|(_, at)| now.duration_since(at) > STALL)
                {
                    if let Some((unanswered, at)) = path.probe {
                        tracing::debug!(
                            session = %inner.label(),
                            path = path.id,
                            nonce = unanswered,
                            waited_ms = now.duration_since(at).as_millis() as u64,
                            "a probe went unanswered; sending another"
                        );
                    }
                    let mut ping = vec![link_tag::PING];
                    ping.extend_from_slice(&nonce.to_be_bytes());
                    if path.control().try_send(ping).is_ok() {
                        path.probe = Some((nonce, now));
                        path.last_probe = now;
                    }
                }
            }
            ack_due = state.ack_due;
        }
        if let Some((path, waited)) = overdue {
            inner.round_trip(path, waited, true, true);
        }

        for (id, quiet) in silent {
            inner.path_failed(id, &format!("silent for {} ms", quiet.as_millis()));
        }
        if let Some(id) = degraded {
            tracing::info!(
                session = %inner.label(),
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
