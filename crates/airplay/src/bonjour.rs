//! Registering a service with macOS' own mDNS responder.
//!
//! macOS will not let a second process answer mDNS the way `mdns-sd` does on
//! Linux and Windows: `mDNSResponder` owns the port and the records, and a
//! service an iPhone can actually see has to be registered *through* it. That
//! is what `DNSServiceRegister` is for, and it is why this platform has its own
//! path at all.
//!
//! ## Why the FFI is here rather than in a crate
//!
//! The obvious crate for this (`astro-dnssd`) creates the reply channel as a
//! local in its registration function, so the receiver is dropped the moment
//! that function returns while the sender lives on in the C callback context
//! for the life of the service. Bonjour calls back **more than once** — a name
//! taken by another machine is answered by a rename, and the rename is a second
//! callback — and that second `send` fails, is `unwrap`ped, and panics inside
//! an `extern "C"` function. A panic there cannot unwind, so the runtime turns
//! it into an abort: the whole daemon dies because two machines chose the same
//! name.
//!
//! Owning the callback is the fix. It does not allocate, does not unwrap, and
//! cannot fail; the worst case is a log line saying the responder renamed us.
//!
//! ## Renaming is expected, not an error
//!
//! Two Cursio nodes announcing one name is the *intended* arrangement — the
//! mesh offers a single receiver — and a brief overlap while one stands down is
//! normal. Bonjour resolves it by renaming, which is exactly right, so the
//! callback reports the name that was actually published rather than treating
//! the difference as a fault.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::AirPlayError;

/// A live registration. Dropping it withdraws the service.
pub struct BonjourService {
    /// Tells the polling thread to stop touching the reference before it is
    /// deallocated. Ordering matters: the thread must be out of
    /// `DNSServiceProcessResult` before `DNSServiceRefDeallocate` runs, or it
    /// would process against freed memory.
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl BonjourService {
    /// Publish `instance` of `service_type` on `port`, with `properties` as its
    /// TXT record.
    pub fn register(
        service_type: &str,
        instance: &str,
        port: u16,
        properties: &[(String, String)],
    ) -> Result<Self, AirPlayError> {
        let c_type = CString::new(service_type)
            .map_err(|_| AirPlayError::Mdns("service type contains a NUL".into()))?;
        let c_name = CString::new(instance)
            .map_err(|_| AirPlayError::Mdns("service name contains a NUL".into()))?;
        let txt = encode_txt(properties)?;

        let mut reference: *mut c_void = std::ptr::null_mut();
        // SAFETY: every pointer is either null (meaning "default") or a
        // NUL-terminated string that outlives the call; `txt` outlives it too,
        // since the responder copies the record before returning.
        let status = unsafe {
            DNSServiceRegister(
                &mut reference,
                0,
                0,
                c_name.as_ptr(),
                c_type.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                // The one argument the API wants in network byte order.
                port.to_be(),
                txt.len() as u16,
                txt.as_ptr().cast::<c_void>(),
                register_reply,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(AirPlayError::Mdns(format!(
                "mDNSResponder refused to register {instance} ({service_type}): error {status}"
            )));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let reference = SendRef(reference);
        let handle = std::thread::Builder::new()
            .name("cursio-bonjour".into())
            .spawn(move || poll(reference, thread_stop))
            .map_err(|error| {
                AirPlayError::Mdns(format!("could not start the mDNS thread: {error}"))
            })?;

        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for BonjourService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Joined rather than detached: the thread deallocates the reference, and
        // returning from here while it still holds one would let the process
        // exit out from under it.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The `DNSServiceRef` handed to the polling thread.
///
/// Not `Send` by nature — it is a raw pointer — but exactly one thread ever
/// touches it, and it is deallocated there. The wrapper states that rather than
/// leaving it to a cast.
struct SendRef(*mut c_void);

// SAFETY: ownership moves to the polling thread at construction and never
// returns; nothing else reads or writes the pointer afterwards.
unsafe impl Send for SendRef {}

/// Drive the registration until the service is dropped.
///
/// Polled with a timeout rather than blocking in `DNSServiceProcessResult`,
/// because that call returns only when the responder has something to say — and
/// a service that is never renamed has nothing to say for hours, which would
/// leave the thread wedged in a syscall at shutdown.
fn poll(reference: SendRef, stop: Arc<AtomicBool>) {
    // SAFETY: `reference` is a live DNSServiceRef owned solely by this thread,
    // deallocated once at the end.
    unsafe {
        let fd = DNSServiceRefSockFD(reference.0);
        while !stop.load(Ordering::SeqCst) {
            let mut entry = PollFd {
                fd,
                events: POLLIN,
                revents: 0,
            };
            let ready = libc_poll(&mut entry, 1, POLL_TIMEOUT_MS);
            if ready > 0 && entry.revents & POLLIN != 0 {
                let status = DNSServiceProcessResult(reference.0);
                if status != 0 {
                    tracing::warn!(status, "mDNS registration ended");
                    break;
                }
            }
        }
        DNSServiceRefDeallocate(reference.0);
    }
}

/// How long a poll waits before re-checking the stop flag.
const POLL_TIMEOUT_MS: i32 = 200;

/// Called by the responder when the registration is confirmed or renamed.
///
/// The whole reason this file exists: it allocates nothing, unwraps nothing,
/// and cannot fail. A panic here would be a panic across an FFI boundary, which
/// aborts the process rather than unwinding.
unsafe extern "C" fn register_reply(
    _reference: *mut c_void,
    _flags: u32,
    error_code: i32,
    name: *const c_char,
    service_type: *const c_char,
    _domain: *const c_char,
    _context: *mut c_void,
) {
    if error_code != 0 {
        tracing::warn!(error_code, "mDNS registration reported an error");
        return;
    }
    // SAFETY: the responder passes NUL-terminated strings valid for the call.
    let published = unsafe { borrow(name) };
    let kind = unsafe { borrow(service_type) };
    tracing::info!(name = %published, service = %kind, "announced over Bonjour");
}

/// Read a C string the responder owns, without taking it.
///
/// # Safety
///
/// `ptr` must be null or a NUL-terminated string valid for this call.
unsafe fn borrow(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Pack key/value pairs into a DNS-SD TXT record: each entry is one length
/// byte followed by `key=value`.
fn encode_txt(properties: &[(String, String)]) -> Result<Vec<u8>, AirPlayError> {
    let mut encoded = Vec::new();
    for (key, value) in properties {
        let entry = format!("{key}={value}");
        // The length is a single byte, so an over-long entry cannot be
        // represented at all — better to say so than to publish a record the
        // responder reads as garbage from that point on.
        let length = u8::try_from(entry.len())
            .map_err(|_| AirPlayError::Mdns(format!("TXT entry {key} is longer than 255 bytes")))?;
        encoded.push(length);
        encoded.extend_from_slice(entry.as_bytes());
    }
    Ok(encoded)
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 0x0001;

type RegisterReply = unsafe extern "C" fn(
    *mut c_void,
    u32,
    i32,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);

// `dns_sd` lives in libSystem on macOS, so no extra link attribute is needed.
extern "C" {
    #[allow(clippy::too_many_arguments)]
    fn DNSServiceRegister(
        reference: *mut *mut c_void,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        service_type: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        callback: RegisterReply,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceProcessResult(reference: *mut c_void) -> i32;
    fn DNSServiceRefSockFD(reference: *mut c_void) -> i32;
    fn DNSServiceRefDeallocate(reference: *mut c_void);

    #[link_name = "poll"]
    fn libc_poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_txt_record_is_length_prefixed_key_equals_value() {
        let encoded = encode_txt(&[("ch".into(), "2".into())]).unwrap();
        assert_eq!(encoded, b"\x04ch=2");
    }

    #[test]
    fn entries_follow_one_another_with_their_own_lengths() {
        let encoded = encode_txt(&[("a".into(), "1".into()), ("bb".into(), "22".into())]).unwrap();
        assert_eq!(encoded, b"\x03a=1\x05bb=22");
    }

    /// A length is one byte, so an over-long entry cannot be represented — and
    /// publishing it truncated would make the responder read the rest of the
    /// record as garbage.
    #[test]
    fn an_entry_too_long_to_measure_is_refused() {
        let long = "x".repeat(300);
        assert!(encode_txt(&[("k".into(), long)]).is_err());
    }

    #[test]
    fn no_properties_is_an_empty_record_rather_than_an_error() {
        assert_eq!(encode_txt(&[]).unwrap(), Vec::<u8>::new());
    }
}
