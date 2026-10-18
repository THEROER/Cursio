//! SBC decoding through the system's `libsbc`, opened when a stream starts.
//!
//! Loaded with `dlopen` rather than linked: BlueZ and PipeWire already put
//! `libsbc.so.1` on every desktop that has Bluetooth audio, a build needs no
//! headers for it, and a machine without it loses only the phone's music —
//! with an error naming the package — rather than failing to start.

use std::ffi::{c_int, c_ulong, c_void, CStr};

/// `sbc_t` from `<sbc/sbc.h>`; stable since libsbc 1.0.
#[repr(C)]
struct SbcT {
    flags: c_ulong,
    frequency: u8,
    blocks: u8,
    subbands: u8,
    mode: u8,
    allocation: u8,
    bitpool: u8,
    endian: u8,
    private: *mut c_void,
    private_alloc_base: *mut c_void,
}

type InitA2dp = unsafe extern "C" fn(*mut SbcT, c_ulong, *const c_void, usize) -> c_int;
type Decode =
    unsafe extern "C" fn(*mut SbcT, *const c_void, usize, *mut c_void, usize, *mut usize) -> isize;
type Finish = unsafe extern "C" fn(*mut SbcT);

pub struct Decoder {
    sbc: Box<SbcT>,
    decode: Decode,
    finish: Finish,
    library: *mut c_void,
}

// SAFETY: the decoder state is owned by this value alone and only touched
// through &mut self; libsbc keeps no thread-local state.
unsafe impl Send for Decoder {}

fn symbol(library: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: library is a live dlopen handle and name is NUL-terminated.
    let sym = unsafe { libc::dlsym(library, name.as_ptr()) };
    if sym.is_null() {
        Err(format!("libsbc has no {}", name.to_string_lossy()))
    } else {
        Ok(sym)
    }
}

impl Decoder {
    /// A decoder for the A2DP configuration the phone and this sink agreed.
    pub fn new(config: &[u8]) -> Result<Self, String> {
        // SAFETY: plain dlopen of a library name.
        let library = unsafe { libc::dlopen(c"libsbc.so.1".as_ptr(), libc::RTLD_NOW) };
        if library.is_null() {
            return Err(
                "libsbc is not installed (package libsbc1); the phone's sound cannot be decoded"
                    .into(),
            );
        }
        let loaded = (|| {
            // SAFETY: each symbol has the signature `<sbc/sbc.h>` declares.
            unsafe {
                Ok::<_, String>((
                    std::mem::transmute::<*mut c_void, InitA2dp>(symbol(
                        library,
                        c"sbc_init_a2dp",
                    )?),
                    std::mem::transmute::<*mut c_void, Decode>(symbol(library, c"sbc_decode")?),
                    std::mem::transmute::<*mut c_void, Finish>(symbol(library, c"sbc_finish")?),
                ))
            }
        })();
        let (init, decode, finish) = match loaded {
            Ok(fns) => fns,
            Err(e) => {
                // SAFETY: the handle came from dlopen above.
                unsafe { libc::dlclose(library) };
                return Err(e);
            }
        };
        let mut sbc = Box::new(SbcT {
            flags: 0,
            frequency: 0,
            blocks: 0,
            subbands: 0,
            mode: 0,
            allocation: 0,
            bitpool: 0,
            endian: 0,
            private: std::ptr::null_mut(),
            private_alloc_base: std::ptr::null_mut(),
        });
        // SAFETY: sbc points to a zeroed sbc_t; config is read for its length.
        let r = unsafe { init(&mut *sbc, 0, config.as_ptr().cast(), config.len()) };
        if r < 0 {
            // SAFETY: as above.
            unsafe { libc::dlclose(library) };
            return Err(format!(
                "libsbc refused the configuration {config:02x?} ({r})"
            ));
        }
        Ok(Decoder {
            sbc,
            decode,
            finish,
            library,
        })
    }

    /// Decode every whole frame in `frames` to interleaved 16-bit PCM.
    pub fn decode(&mut self, mut frames: &[u8], pcm: &mut Vec<i16>) {
        let mut out = [0i16; 4096];
        while !frames.is_empty() {
            let mut written = 0usize;
            // SAFETY: input and output buffers are valid for the lengths
            // passed; written is a valid out-pointer.
            let used = unsafe {
                (self.decode)(
                    &mut *self.sbc,
                    frames.as_ptr().cast(),
                    frames.len(),
                    out.as_mut_ptr().cast(),
                    std::mem::size_of_val(&out),
                    &mut written,
                )
            };
            if used <= 0 {
                if used < 0 {
                    tracing::debug!(error = used, "an undecodable SBC frame; dropped");
                }
                return;
            }
            pcm.extend_from_slice(&out[..written / 2]);
            frames = &frames[(used as usize).min(frames.len())..];
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: sbc was initialised by sbc_init_a2dp; the library handle is
        // closed only after the last call into it.
        unsafe {
            (self.finish)(&mut *self.sbc);
            libc::dlclose(self.library);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Encode = unsafe extern "C" fn(
        *mut SbcT,
        *const c_void,
        usize,
        *mut c_void,
        usize,
        *mut isize,
    ) -> isize;

    /// Encode with libsbc's own encoder, so the decoder is checked against
    /// the frames the library itself writes, through this crate's `sbc_t`.
    fn encode(config: &[u8], pcm: &[i16]) -> Vec<u8> {
        let mut enc = Decoder::new(config).expect("libsbc is installed where Bluetooth audio is");
        // SAFETY: sbc_encode's signature as `<sbc/sbc.h>` declares it.
        let encode: Encode =
            unsafe { std::mem::transmute(symbol(enc.library, c"sbc_encode").unwrap()) };
        // SAFETY: reinterpreting initialised i16s as bytes.
        let mut input: &[u8] =
            unsafe { std::slice::from_raw_parts(pcm.as_ptr().cast(), std::mem::size_of_val(pcm)) };
        let mut out = Vec::new();
        let mut frame = [0u8; 512];
        while !input.is_empty() {
            let mut written: isize = 0;
            // SAFETY: buffers are valid for the lengths passed.
            let used = unsafe {
                encode(
                    &mut *enc.sbc,
                    input.as_ptr().cast(),
                    input.len(),
                    frame.as_mut_ptr().cast(),
                    frame.len(),
                    &mut written,
                )
            };
            if used <= 0 {
                break;
            }
            out.extend_from_slice(&frame[..written as usize]);
            input = &input[used as usize..];
        }
        out
    }

    #[test]
    fn a_tone_survives_libsbc_through_this_binding() {
        let config = crate::a2dp::select_sbc(&[0xFF, 0xFF, 2, 53]).unwrap();
        let tone: Vec<i16> = (0..4800)
            .flat_map(|i| {
                let v =
                    ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect();
        let frames = encode(&config, &tone);
        assert!(!frames.is_empty());
        let mut decoder = Decoder::new(&config).unwrap();
        let mut pcm = Vec::new();
        decoder.decode(&frames, &mut pcm);
        assert!(
            pcm.len() > tone.len() * 9 / 10,
            "decoded {} of {}",
            pcm.len(),
            tone.len()
        );
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((6000..10000).contains(&peak), "peak {peak}");
    }
}
