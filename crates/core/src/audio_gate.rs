//! Which captured chunks an outbound audio stream bothers to send.
//!
//! Two different questions share this gate, and answering both with one
//! threshold is what cut people off mid-sentence.
//!
//! An **application stream** asks "is this source idle?". A virtual sink
//! produces chunks whether or not anything plays into it, and every idle mirror
//! pumping full-rate silence is link capacity the cursor shares. The answer is
//! a fixed level just above the digital noise floor ([`SILENCE_THRESHOLD`]):
//! programme material is never that quiet, and an adaptive threshold would chop
//! the soft passages of music it has decided are "background".
//!
//! A **microphone** asks "is someone speaking?", and an absolute level cannot
//! answer that: the same voice lands 20 dB apart on two microphones at two input
//! gains. At -60 dBFS a quiet speaker on a modestly set mic was below the line
//! for whole words. So a mic's threshold follows its own noise floor — tracked
//! slow-rise/fast-fall, the usual VAD noise estimator, so a dip between words
//! pulls it down at once while speech raises it only gradually — and sits a
//! margin above it, inside bounds that keep it from either passing hiss or
//! cutting ordinary speech in a loud room. libopus' own DTX judges activity the
//! same way, against the signal's own level rather than an absolute one; it is
//! not used directly because a dropped chunk costs the link nothing, while a DTX
//! frame still costs a packet and a session-loop send.
//!
//! Both kinds then hold the gate open for [`HANGOVER`] after the last sound. A
//! chunk is a few milliseconds, so judging each alone dropped the decay of every
//! word and every gap between syllables; each one reached the receiver as an
//! underrun, faded out and back in, and was heard as chopped speech.

use std::str::FromStr;

/// Bytes per second of the capture format: 48 kHz, stereo, f32.
const BYTES_PER_SECOND: f32 = 48_000.0 * 2.0 * 4.0;

/// Below this absolute sample value an application chunk counts as silence.
///
/// Not exactly zero: a real capture path picks up dither and denormal noise, so
/// an idle device rarely produces bit-perfect zeros.
///
/// **-60 dBFS**, and the exact value matters. It used to be 3.0e-5 (~-90 dBFS),
/// which sits *below* one LSB of 16-bit audio (3.052e-5) — so a single flickering
/// low bit, which is exactly what idle VoiceMeeter/VB-Cable endpoints produce,
/// counted as sound. Measured effect: 477000 chunks sent versus 731 recognised
/// as silent, i.e. the check was doing nothing while ~15 idle mirrors each
/// pumped a full-rate stream. That flooded the send budget (over_budget=15241),
/// and since input shares the channel it was heard as stuttering, crackling
/// audio and felt as laggy input.
///
/// -60 dBFS is the conventional idle threshold for this job (snapcast uses a
/// dBFS threshold the same way): far below anything audible in program material,
/// yet a comfortable ~30 dB above the dither floor, so idle really reads as idle.
pub const SILENCE_THRESHOLD: f32 = 1.0e-3;

/// How long a stream keeps sending after it was last audible.
const HANGOVER_SECS: f32 = 1.0;
const HANGOVER: usize = (HANGOVER_SECS * BYTES_PER_SECOND) as usize;

/// How far above the noise floor a microphone must rise to count as speech.
const MIC_MARGIN_DB: f32 = 9.0;
/// Lowest automatic threshold. A muted or digitally silent mic drives the floor
/// to nothing; below this the gate would open on dither.
const MIC_THRESHOLD_MIN_DB: f32 = -72.0;
/// Highest automatic threshold. Conversational speech sits around -30 dBFS RMS;
/// past this a noisy room would start gating a normal voice.
const MIC_THRESHOLD_MAX_DB: f32 = -42.0;
/// How fast the noise floor may rise while the level stays above it. Slow
/// enough that a sentence does not lift it into the voice, fast enough that a
/// fan switched on is learned within seconds.
const FLOOR_RISE_DB_PER_SEC: f32 = 1.5;
/// Level reported for a chunk of pure zeros, which has no logarithm.
const LEVEL_OF_ZEROS_DB: f32 = -120.0;

/// How a microphone decides what counts as someone speaking. Configured as
/// `audio.mic_gate`: `auto`, `off`, or a level in dBFS such as `-55`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum MicGate {
    /// Follow the mic's own noise floor.
    #[default]
    Auto,
    /// Send everything while anyone records from the mic.
    Off,
    /// Speech is anything at or above this RMS level, in dBFS.
    Fixed(f32),
}

impl FromStr for MicGate {
    type Err = crate::config::ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            crate::config::ConfigError::Invalid(format!(
                "audio.mic_gate is {s:?}; use \"auto\", \"off\" or a level in dBFS between -100 and 0, such as \"-55\""
            ))
        };
        match s.trim() {
            "auto" => Ok(Self::Auto),
            "off" => Ok(Self::Off),
            level => {
                let db: f32 = level
                    .trim_end_matches("dBFS")
                    .trim_end_matches("dB")
                    .trim()
                    .parse()
                    .map_err(|_| invalid())?;
                if (-100.0..=0.0).contains(&db) {
                    Ok(Self::Fixed(db))
                } else {
                    Err(invalid())
                }
            }
        }
    }
}

impl MicGate {
    /// The configured value, where `None` is the default.
    pub fn from_config(value: Option<&str>) -> Result<Self, crate::config::ConfigError> {
        value.map_or(Ok(Self::Auto), str::parse)
    }
}

/// What a stream's chunks are judged as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GateKind {
    /// An application or device mirror: drop it once it is idle.
    Program,
    /// A microphone: drop it while nobody speaks.
    Mic(MicGate),
}

/// One outbound stream's gate.
#[derive(Debug, Default)]
pub struct Gate {
    /// Bytes of silence since the stream was last audible; `None` until it
    /// first is, so an idle mirror is dropped from its first chunk.
    quiet: Option<usize>,
    /// The mic's estimated noise floor in dBFS, once it has heard a chunk.
    floor_db: Option<f32>,
}

impl Gate {
    /// Whether this chunk of f32 little-endian samples is sent.
    pub fn admit(&mut self, kind: GateKind, data: &[u8]) -> bool {
        let audible = match kind {
            GateKind::Program => !is_silent(data),
            GateKind::Mic(MicGate::Off) => true,
            GateKind::Mic(MicGate::Fixed(threshold)) => rms_db(data) >= threshold,
            GateKind::Mic(MicGate::Auto) => {
                let level = rms_db(data);
                let threshold = self.auto_threshold();
                self.track_floor(level, data.len());
                level >= threshold
            }
        };
        if audible {
            self.quiet = Some(0);
            return true;
        }
        match &mut self.quiet {
            Some(quiet) if *quiet < HANGOVER => {
                *quiet += data.len();
                true
            }
            _ => false,
        }
    }

    /// The automatic mic threshold as it stands, for the log.
    pub fn auto_threshold(&self) -> f32 {
        self.floor_db.map_or(MIC_THRESHOLD_MIN_DB, |floor| {
            (floor + MIC_MARGIN_DB).clamp(MIC_THRESHOLD_MIN_DB, MIC_THRESHOLD_MAX_DB)
        })
    }

    pub fn floor_db(&self) -> Option<f32> {
        self.floor_db
    }

    fn track_floor(&mut self, level: f32, bytes: usize) {
        let rise = FLOOR_RISE_DB_PER_SEC * bytes as f32 / BYTES_PER_SECOND;
        self.floor_db = Some(match self.floor_db {
            Some(floor) if level > floor => (floor + rise).min(level),
            _ => level,
        });
    }
}

/// Is this chunk (f32 little-endian samples) inaudible as programme material?
fn is_silent(data: &[u8]) -> bool {
    data.as_chunks::<4>()
        .0
        .iter()
        .all(|b| f32::from_le_bytes(*b).abs() < SILENCE_THRESHOLD)
}

/// RMS level of a chunk of f32 little-endian samples, in dBFS.
pub fn rms_db(data: &[u8]) -> f32 {
    let samples = data.as_chunks::<4>().0;
    if samples.is_empty() {
        return LEVEL_OF_ZEROS_DB;
    }
    let power = samples
        .iter()
        .map(|b| {
            let s = f32::from_le_bytes(*b);
            s * s
        })
        .sum::<f32>()
        / samples.len() as f32;
    if power <= 0.0 {
        LEVEL_OF_ZEROS_DB
    } else {
        (10.0 * power.log10()).max(LEVEL_OF_ZEROS_DB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 ms of a 1 kHz tone whose RMS is `db` dBFS.
    fn tone(db: f32) -> Vec<u8> {
        let amplitude = 10f32.powf(db / 20.0) * std::f32::consts::SQRT_2;
        (0..480)
            .flat_map(|i| {
                let s = amplitude * (i as f32 * 2.0 * std::f32::consts::PI / 48.0).sin();
                [s, s]
            })
            .flat_map(f32::to_le_bytes)
            .collect()
    }

    fn feed(gate: &mut Gate, kind: GateKind, chunk: &[u8], times: usize) -> usize {
        (0..times).filter(|_| gate.admit(kind, chunk)).count()
    }

    const CHUNKS_PER_SEC: usize = 100;
    const HANGOVER_CHUNKS: usize = (HANGOVER_SECS * CHUNKS_PER_SEC as f32) as usize;

    /// Regression guard: it was once 3.0e-5, just *below* an s16 LSB (3.052e-5),
    /// so a single flickering low bit — what idle virtual cables emit all day —
    /// read as sound. Every idle mirror then streamed at full rate, exhausted the
    /// send budget, and the audio the user actually wanted arrived stuttering.
    #[test]
    fn silence_threshold_is_above_the_16_bit_noise_floor() {
        const S16_LSB: f32 = 1.0 / 32768.0;
        const {
            assert!(
                SILENCE_THRESHOLD > S16_LSB,
                "silence threshold must exceed one s16 LSB"
            );
        }
    }

    /// ...and still well below anything a listener would call audible, so real
    /// quiet passages are never dropped.
    #[test]
    fn silence_threshold_still_passes_quiet_audio() {
        let quiet = 10f32.powf(-50.0 / 20.0);
        assert!(
            quiet > SILENCE_THRESHOLD,
            "quiet audio at -50 dBFS ({quiet:e}) must not be classified as silence"
        );
    }

    #[test]
    fn is_silent_detects_idle_and_active_streams() {
        let lsb = (1.0f32 / 32768.0).to_le_bytes();
        let idle: Vec<u8> = lsb.iter().cycle().take(4 * 64).copied().collect();
        assert!(is_silent(&idle), "one-LSB noise should count as silence");

        let mut active = idle.clone();
        active[..4].copy_from_slice(&0.5f32.to_le_bytes());
        assert!(
            !is_silent(&active),
            "a loud sample must mark the chunk as audio"
        );
    }

    #[test]
    fn rms_of_a_tone_reads_its_level() {
        assert!((rms_db(&tone(-30.0)) + 30.0).abs() < 0.1);
        assert_eq!(rms_db(&[0u8; 64]), LEVEL_OF_ZEROS_DB);
    }

    #[test]
    fn a_stream_that_never_sounded_is_dropped_from_its_first_chunk() {
        let mut gate = Gate::default();
        assert!(!gate.admit(GateKind::Program, &[0u8; 3840]));
    }

    #[test]
    fn quiet_stretches_inside_sound_are_sent_until_the_hangover_runs_out() {
        let mut gate = Gate::default();
        let silence = tone(-100.0);
        assert!(gate.admit(GateKind::Program, &tone(-20.0)));
        assert_eq!(
            feed(&mut gate, GateKind::Program, &silence, HANGOVER_CHUNKS + 5),
            HANGOVER_CHUNKS,
            "exactly the hangover's worth of silence follows the sound"
        );
        assert!(
            gate.admit(GateKind::Program, &tone(-20.0)),
            "sound reopens it"
        );
        assert!(
            gate.admit(GateKind::Program, &silence),
            "and restarts the hangover"
        );
    }

    /// The reported bug: a voice under -60 dBFS on a quiet mic was cut whole.
    #[test]
    fn quiet_speech_over_a_quieter_floor_passes_the_automatic_mic_gate() {
        let mut gate = Gate::default();
        let kind = GateKind::Mic(MicGate::Auto);
        feed(&mut gate, kind, &tone(-80.0), 3 * CHUNKS_PER_SEC);
        assert_eq!(
            feed(&mut gate, kind, &tone(-64.0), 2 * CHUNKS_PER_SEC),
            2 * CHUNKS_PER_SEC,
            "speech 16 dB over the floor is speech, however quiet in absolute terms"
        );
    }

    #[test]
    fn the_automatic_mic_gate_closes_on_steady_background_noise() {
        let mut gate = Gate::default();
        let kind = GateKind::Mic(MicGate::Auto);
        let fan = tone(-55.0);
        let sent = feed(&mut gate, kind, &fan, 10 * CHUNKS_PER_SEC);
        assert!(
            !gate.admit(kind, &fan),
            "a steady fan is learned as the floor"
        );
        assert!(sent <= HANGOVER_CHUNKS + 1, "sent {sent} chunks of a fan");
    }

    #[test]
    fn a_sentence_does_not_lift_the_floor_into_the_voice() {
        let mut gate = Gate::default();
        let kind = GateKind::Mic(MicGate::Auto);
        feed(&mut gate, kind, &tone(-75.0), CHUNKS_PER_SEC);
        assert_eq!(
            feed(&mut gate, kind, &tone(-50.0), 5 * CHUNKS_PER_SEC),
            5 * CHUNKS_PER_SEC,
            "five seconds of unbroken speech stays open"
        );
    }

    #[test]
    fn a_loud_room_never_raises_the_mic_threshold_past_normal_speech() {
        let mut gate = Gate::default();
        let kind = GateKind::Mic(MicGate::Auto);
        feed(&mut gate, kind, &tone(-35.0), 60 * CHUNKS_PER_SEC);
        assert_eq!(gate.auto_threshold(), MIC_THRESHOLD_MAX_DB);
        assert!(gate.admit(kind, &tone(-30.0)));
    }

    #[test]
    fn a_fixed_mic_gate_uses_the_configured_level() {
        let mut gate = Gate::default();
        let kind = GateKind::Mic(MicGate::Fixed(-50.0));
        assert!(!gate.admit(kind, &tone(-55.0)));
        assert!(gate.admit(kind, &tone(-45.0)));
    }

    #[test]
    fn an_open_mic_gate_sends_digital_silence() {
        let mut gate = Gate::default();
        assert!(gate.admit(GateKind::Mic(MicGate::Off), &[0u8; 3840]));
    }

    #[test]
    fn mic_gate_parses_its_three_spellings_and_refuses_the_rest() {
        assert_eq!("auto".parse::<MicGate>().unwrap(), MicGate::Auto);
        assert_eq!("off".parse::<MicGate>().unwrap(), MicGate::Off);
        assert_eq!("-55".parse::<MicGate>().unwrap(), MicGate::Fixed(-55.0));
        assert_eq!(
            "-48 dBFS".parse::<MicGate>().unwrap(),
            MicGate::Fixed(-48.0)
        );
        assert_eq!(MicGate::from_config(None).unwrap(), MicGate::Auto);
        for bad in ["loud", "10", "-200", ""] {
            assert!(bad.parse::<MicGate>().is_err(), "{bad:?} must be refused");
        }
    }
}
