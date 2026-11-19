//! A named pipe only this user can open: the control socket's on Windows,
//! and the drive helper's.
//!
//! Here, below both, because the drive helper lives with the files module
//! and the control socket with the node, and each needs the same protected
//! DACL. The errors read exactly as the control socket's always did.

use std::path::Path;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Why a pipe could not be created.
#[derive(Debug, thiserror::Error)]
pub enum OwnerPipeError {
    #[error("control socket i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("a cursio node is already running for this user (socket {0})")]
    AlreadyRunning(String),
}

fn os_error(e: windows::core::Error) -> OwnerPipeError {
    OwnerPipeError::Io(std::io::Error::other(e))
}

/// This process's user SID in `S-1-5-…` form. The token's *user*, not
/// its owner: an elevated token's default owner is Administrators, which
/// the unelevated app is not acting as.
fn user_sid() -> Result<String, OwnerPipeError> {
    // SAFETY: every handle opened here is closed before returning, the
    // buffer is sized by the first GetTokenInformation call, and the SID
    // string is freed with LocalFree as the API requires.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(os_error)?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        let got = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        got.map_err(os_error)?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(os_error)?;
        let sid = text
            .to_string()
            .map_err(|e| OwnerPipeError::Io(std::io::Error::other(e)));
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

pub fn create(endpoint: &Path, first: bool) -> Result<NamedPipeServer, OwnerPipeError> {
    // Protected DACL: this user and SYSTEM, full access; nobody else. The
    // medium no-write-up label lets an unelevated client in when the node
    // itself is elevated.
    let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;SY)S:(ML;;NW;;;ME)", user_sid()?);
    let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the
    // descriptor is freed with LocalFree once the pipe has copied it.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(os_error)?;
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: `attributes` points at a valid descriptor for the duration
    // of the call, which copies it into the pipe object.
    let created = unsafe {
        options.create_with_security_attributes_raw(
            endpoint.as_os_str(),
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    };
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    created.map_err(
        |e| match first && e.kind() == std::io::ErrorKind::PermissionDenied {
            true => OwnerPipeError::AlreadyRunning(endpoint.display().to_string()),
            false => OwnerPipeError::Io(e),
        },
    )
}
