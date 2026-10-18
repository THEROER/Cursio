//! The hands-free side of one HFP link, without the socket.
//!
//! Lines in, lines out: [`HandsFree::feed`] takes what the phone sent and
//! answers with what to write back and what happened. Keeping the socket out
//! is what lets every exchange below be tested on dialogues recorded from
//! real phones, with no adapter and no phone.
//!
//! ## One command at a time
//!
//! An AT command is complete only at its final `OK` or `ERROR`, and the
//! result codes in between belong to it. Writing the next command before then
//! makes the phone's answers impossible to attribute, so commands queue and
//! the next goes out on the previous one's final result.
//!
//! ## Calls come from `+CLCC` when the phone offers it
//!
//! Indicators (`call`, `callsetup`, `callheld`) say *that* something changed
//! but not which call or whose number. A phone with enhanced call status
//! answers `AT+CLCC` with every call, its direction, state and number — the
//! same list the mesh module carries — so after every indicator change the
//! list is asked for again. Without it the list is rebuilt from the
//! indicators and the last `+CLIP`, which is right for the one-call case and
//! all a phone without `+CLCC` can give.
//!
//! ## Narrowband only, on purpose
//!
//! Codec negotiation (mSBC wideband) is not advertised, so the phone uses
//! CVSD: 8 kHz, which the controller turns into linear PCM on the SCO socket
//! and needs no codec here. mSBC needs a transparent SCO path and an SBC codec
//! on this side; offering it before both exist would have the phone pick a
//! codec the audio path cannot decode.

use std::collections::VecDeque;

use cursio_protocol::calls::{Call, CallDirection, CallState};

use crate::at::{self, AgLine};

/// What this unit says it supports in `AT+BRSF`: caller id (bit 2), remote
/// volume (bit 4), enhanced call status (bit 5). Three-way calling, voice
/// recognition and codec negotiation are deliberately absent.
pub const HF_FEATURES: u32 = (1 << 2) | (1 << 4) | (1 << 5);

/// The phone's `+BRSF` bit for enhanced call status (`AT+CLCC`).
const AG_ENHANCED_CALL_STATUS: u32 = 1 << 6;

/// Something the link's owner needs to act on or show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfEvent {
    /// The service level connection is up: calls can be read and controlled.
    Connected,
    /// Every call the phone has now; empty when there are none.
    Calls(Vec<Call>),
    /// A command the user asked for failed on the phone.
    Refused(String),
}

/// What to do with the link after feeding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfOutput {
    /// Write this line, terminator included.
    Send(String),
    Event(HfEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Features,
    IndicatorNames,
    IndicatorValues,
    Reporting,
    Ready,
}

/// A command in flight, and what its final result means.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Setup,
    /// One the user asked for; its `ERROR` is worth telling them about.
    User(&'static str),
    /// `AT+CLCC`: the listing gathers until `OK`.
    List,
    Quiet,
}

pub struct HandsFree {
    phase: Phase,
    queue: VecDeque<(String, Pending)>,
    in_flight: Option<Pending>,
    ag_features: u32,
    names: Vec<String>,
    values: Vec<u32>,
    clip: (Option<String>, Option<String>),
    listing: Vec<at::ClccEntry>,
    calls: Vec<Call>,
}

impl Default for HandsFree {
    fn default() -> Self {
        Self::new()
    }
}

impl HandsFree {
    pub fn new() -> Self {
        HandsFree {
            phase: Phase::Features,
            queue: VecDeque::new(),
            in_flight: None,
            ag_features: 0,
            names: Vec::new(),
            values: Vec::new(),
            clip: (None, None),
            listing: Vec::new(),
            calls: Vec::new(),
        }
    }

    /// The first line on a new link: the hands-free unit speaks first.
    pub fn start(&mut self) -> Vec<HfOutput> {
        self.command(format!("AT+BRSF={HF_FEATURES}"), Pending::Setup)
    }

    pub fn is_ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    pub fn calls(&self) -> &[Call] {
        &self.calls
    }

    pub fn answer(&mut self) -> Vec<HfOutput> {
        self.command("ATA".into(), Pending::User("answer"))
    }

    /// Decline a ringing call or end one that is up: HFP has one command for
    /// both, and the phone applies it to whichever there is.
    pub fn hang_up(&mut self) -> Vec<HfOutput> {
        self.command("AT+CHUP".into(), Pending::User("hang up"))
    }

    pub fn dial(&mut self, number: &str) -> Vec<HfOutput> {
        let number: String = number
            .chars()
            .filter(|c| c.is_ascii_digit() || matches!(c, '+' | '*' | '#'))
            .collect();
        if number.is_empty() {
            return vec![HfOutput::Event(HfEvent::Refused(
                "that is not a phone number".into(),
            ))];
        }
        self.command(format!("ATD{number};"), Pending::User("dial"))
    }

    /// Feed one line from the phone.
    pub fn feed(&mut self, line: &str) -> Vec<HfOutput> {
        let mut out = Vec::new();
        match at::parse(line) {
            AgLine::Ok => self.finished(true, None, &mut out),
            AgLine::Error(code) => self.finished(false, code, &mut out),
            AgLine::Brsf(features) => self.ag_features = features,
            AgLine::CindNames(names) => self.names = names,
            AgLine::CindValues(values) => self.values = values,
            AgLine::Ciev { index, value } => {
                if let Some(slot) = index.checked_sub(1).and_then(|i| self.values.get_mut(i)) {
                    *slot = value;
                }
                if self.indicator("call") == Some(0) && self.indicator("callsetup") == Some(0) {
                    self.clip = (None, None);
                }
                self.refresh(&mut out);
            }
            AgLine::Ring => {
                if self.calls.is_empty() {
                    self.refresh(&mut out);
                }
            }
            AgLine::Clip { number, name } => {
                if self.clip != (number.clone(), name.clone()) {
                    self.clip = (number, name);
                    self.refresh(&mut out);
                }
            }
            AgLine::Clcc(entry) => {
                if self.in_flight == Some(Pending::List) {
                    self.listing.push(entry);
                }
            }
            AgLine::Other(line) => tracing::debug!(%line, "unhandled line from the phone"),
            AgLine::Chld | AgLine::Volume { .. } | AgLine::Bcs(_) => {}
        }
        out
    }

    fn command(&mut self, line: String, pending: Pending) -> Vec<HfOutput> {
        self.queue.push_back((line, pending));
        let mut out = Vec::new();
        self.send_next(&mut out);
        out
    }

    fn send_next(&mut self, out: &mut Vec<HfOutput>) {
        if self.in_flight.is_some() {
            return;
        }
        if let Some((line, pending)) = self.queue.pop_front() {
            if pending == Pending::List {
                self.listing.clear();
            }
            self.in_flight = Some(pending);
            out.push(HfOutput::Send(format!("{line}\r")));
        }
    }

    fn finished(&mut self, ok: bool, code: Option<u32>, out: &mut Vec<HfOutput>) {
        let pending = self.in_flight.take();
        match pending {
            Some(Pending::Setup) => self.advance(ok, out),
            Some(Pending::User(what)) if !ok => {
                let reason = match code {
                    Some(code) => format!("the phone refused to {what} (error {code})"),
                    None => format!("the phone refused to {what}"),
                };
                out.push(HfOutput::Event(HfEvent::Refused(reason)));
            }
            Some(Pending::List) => {
                let calls = if ok {
                    let listing = std::mem::take(&mut self.listing);
                    listing
                        .into_iter()
                        .map(|e| self.call_from_listing(e))
                        .collect()
                } else {
                    self.calls_from_indicators()
                };
                self.publish(calls, out);
            }
            _ => {}
        }
        self.send_next(out);
    }

    /// The next step of the service level connection.
    fn advance(&mut self, ok: bool, out: &mut Vec<HfOutput>) {
        let next = match (self.phase, ok) {
            (Phase::Features, _) => {
                // A phone that refuses BRSF is an HFP 0.96 gateway; it still
                // answers the rest, so the setup carries on.
                self.phase = Phase::IndicatorNames;
                Some("AT+CIND=?")
            }
            (Phase::IndicatorNames, true) => {
                self.phase = Phase::IndicatorValues;
                Some("AT+CIND?")
            }
            (Phase::IndicatorValues, true) => {
                self.phase = Phase::Reporting;
                Some("AT+CMER=3,0,0,1")
            }
            (Phase::Reporting, true) => {
                self.phase = Phase::Ready;
                out.push(HfOutput::Event(HfEvent::Connected));
                self.queue.push_back(("AT+CLIP=1".into(), Pending::Quiet));
                self.queue.push_back(("AT+CMEE=1".into(), Pending::Quiet));
                self.refresh(out);
                None
            }
            (phase, false) => {
                tracing::warn!(?phase, "the phone refused hands-free setup");
                out.push(HfOutput::Event(HfEvent::Refused(
                    "the phone refused the hands-free connection".into(),
                )));
                None
            }
            (Phase::Ready, true) => None,
        };
        if let Some(line) = next {
            self.queue.push_front((line.into(), Pending::Setup));
        }
    }

    fn indicator(&self, name: &str) -> Option<u32> {
        let index = self.names.iter().position(|n| n == name)?;
        self.values.get(index).copied()
    }

    /// Bring the call list up to date: ask the phone when it can list them,
    /// else rebuild from what the indicators say.
    fn refresh(&mut self, out: &mut Vec<HfOutput>) {
        if self.phase != Phase::Ready {
            return;
        }
        if self.ag_features & AG_ENHANCED_CALL_STATUS != 0 {
            let queued = self.in_flight == Some(Pending::List)
                || self.queue.iter().any(|(_, p)| *p == Pending::List);
            if !queued {
                self.queue.push_back(("AT+CLCC".into(), Pending::List));
            }
            self.send_next(out);
        } else {
            let calls = self.calls_from_indicators();
            self.publish(calls, out);
        }
    }

    fn publish(&mut self, calls: Vec<Call>, out: &mut Vec<HfOutput>) {
        if calls != self.calls {
            self.calls = calls.clone();
            out.push(HfOutput::Event(HfEvent::Calls(calls)));
        }
    }

    fn call_from_listing(&self, entry: at::ClccEntry) -> Call {
        let state = match entry.status {
            0 => CallState::Active,
            1 => CallState::Held,
            2 => CallState::Dialing,
            3 => CallState::Alerting,
            4 => CallState::Ringing,
            _ => CallState::Waiting,
        };
        let name = (entry.number.is_some() && entry.number == self.clip.0)
            .then(|| self.clip.1.clone())
            .flatten();
        Call {
            id: entry.index,
            direction: if entry.incoming {
                CallDirection::Incoming
            } else {
                CallDirection::Outgoing
            },
            state,
            number: entry.number,
            name,
        }
    }

    fn calls_from_indicators(&self) -> Vec<Call> {
        let call = self.indicator("call").unwrap_or(0);
        let setup = self.indicator("callsetup").unwrap_or(0);
        let held = self.indicator("callheld").unwrap_or(0);
        let (number, name) = self.clip.clone();
        let mut calls = Vec::new();
        if call == 1 {
            calls.push(Call {
                id: 1,
                direction: CallDirection::Incoming,
                state: if held == 2 {
                    CallState::Held
                } else {
                    CallState::Active
                },
                number: number.clone(),
                name: name.clone(),
            });
        }
        let setup_state = match setup {
            1 if call == 1 => Some((CallDirection::Incoming, CallState::Waiting)),
            1 => Some((CallDirection::Incoming, CallState::Ringing)),
            2 => Some((CallDirection::Outgoing, CallState::Dialing)),
            3 => Some((CallDirection::Outgoing, CallState::Alerting)),
            _ => None,
        };
        if let Some((direction, state)) = setup_state {
            calls.push(Call {
                id: 2,
                direction,
                state,
                number,
                name,
            });
        }
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: &str = r#"+CIND: ("call",(0,1)),("callsetup",(0-3)),("service",(0,1)),("signal",(0-5)),("roam",(0,1)),("battchg",(0-5)),("callheld",(0-2))"#;

    fn sent(out: &[HfOutput]) -> Vec<String> {
        out.iter()
            .filter_map(|o| match o {
                HfOutput::Send(s) => Some(s.trim_end().to_string()),
                _ => None,
            })
            .collect()
    }

    fn events(out: &[HfOutput]) -> Vec<HfEvent> {
        out.iter()
            .filter_map(|o| match o {
                HfOutput::Event(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    /// Run the service level setup against a phone with these features.
    fn connected(ag_features: u32) -> HandsFree {
        let mut hf = HandsFree::new();
        assert_eq!(sent(&hf.start()), vec!["AT+BRSF=52"]);
        hf.feed(&format!("+BRSF: {ag_features}"));
        assert_eq!(sent(&hf.feed("OK")), vec!["AT+CIND=?"]);
        hf.feed(NAMES);
        assert_eq!(sent(&hf.feed("OK")), vec!["AT+CIND?"]);
        hf.feed("+CIND: 0,0,1,4,0,3,0");
        assert_eq!(sent(&hf.feed("OK")), vec!["AT+CMER=3,0,0,1"]);
        let out = hf.feed("OK");
        assert!(events(&out).contains(&HfEvent::Connected));
        assert!(hf.is_ready());
        hf
    }

    #[test]
    fn setup_runs_brsf_cind_and_cmer_in_order_and_one_at_a_time() {
        let mut hf = connected(871);
        // Caller id and extended errors follow, still one at a time.
        let out = hf.feed("OK");
        assert_eq!(sent(&out).len(), 1);
    }

    #[test]
    fn a_ringing_call_is_listed_with_its_number_from_clcc() {
        let mut hf = connected(871);
        // Caller id, extended errors and the first listing, all empty.
        for _ in 0..3 {
            hf.feed("OK");
        }
        let out = hf.feed("+CIEV: 2,1");
        assert_eq!(sent(&out), vec!["AT+CLCC"]);
        hf.feed(r#"+CLIP: "+380501234567",145,,,"Mom""#);
        hf.feed(r#"+CLCC: 1,1,4,0,0,"+380501234567",145"#);
        let out = hf.feed("OK");
        assert_eq!(
            events(&out),
            vec![HfEvent::Calls(vec![Call {
                id: 1,
                direction: CallDirection::Incoming,
                state: CallState::Ringing,
                number: Some("+380501234567".into()),
                name: Some("Mom".into()),
            }])]
        );
    }

    #[test]
    fn without_clcc_a_call_is_rebuilt_from_the_indicators() {
        let mut hf = connected(0);
        for _ in 0..2 {
            hf.feed("OK");
        }
        hf.feed(r#"+CLIP: "112",129"#);
        let out = hf.feed("+CIEV: 2,1");
        assert_eq!(
            events(&out),
            vec![HfEvent::Calls(vec![Call {
                id: 2,
                direction: CallDirection::Incoming,
                state: CallState::Ringing,
                number: Some("112".into()),
                name: None,
            }])]
        );
        hf.feed("+CIEV: 1,1");
        let out = hf.feed("+CIEV: 2,0");
        assert_eq!(
            events(&out),
            vec![HfEvent::Calls(vec![Call {
                id: 1,
                direction: CallDirection::Incoming,
                state: CallState::Active,
                number: Some("112".into()),
                name: None,
            }])]
        );
        let out = hf.feed("+CIEV: 1,0");
        assert_eq!(events(&out), vec![HfEvent::Calls(Vec::new())]);
    }

    #[test]
    fn a_refused_answer_is_reported_with_the_phones_error() {
        let mut hf = connected(0);
        for _ in 0..2 {
            hf.feed("OK");
        }
        assert_eq!(sent(&hf.answer()), vec!["ATA"]);
        assert_eq!(
            events(&hf.feed("+CME ERROR: 3")),
            vec![HfEvent::Refused(
                "the phone refused to answer (error 3)".into()
            )]
        );
    }

    #[test]
    fn a_command_waits_for_the_one_in_flight() {
        let mut hf = connected(0);
        // CLIP is in flight; a user's dial must not interleave with it.
        assert!(sent(&hf.dial("+38 050 123")).is_empty());
        assert_eq!(sent(&hf.feed("OK")), vec!["AT+CMEE=1"]);
        assert_eq!(sent(&hf.feed("OK")), vec!["ATD+38050123;"]);
    }

    #[test]
    fn dialing_nothing_dialable_is_refused_without_reaching_the_phone() {
        let mut hf = connected(0);
        let out = hf.dial("call me");
        assert!(sent(&out).is_empty());
        assert_eq!(events(&out).len(), 1);
    }
}
