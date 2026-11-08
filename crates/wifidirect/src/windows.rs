//! Running a group on Windows, through WinRT's Wi-Fi Direct publisher.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementPublisher, WiFiDirectAdvertisementPublisherStatus,
};
use windows::Security::Credentials::PasswordCredential;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    wlan_intf_opcode_channel_number, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory,
    WlanOpenHandle, WlanQueryInterface, WLAN_INTERFACE_INFO_LIST,
};

use crate::{channel_freq_mhz, Ability, DirectError, Result};

/// How long the publisher may take to report the group started.
const START_WITHIN: Duration = Duration::from_secs(10);

fn wfd_err(e: windows::core::Error) -> DirectError {
    DirectError::WiFiDirect(e.message().to_string())
}

pub struct Group {
    pub ssid: String,
    pub passphrase: String,
    publisher: WiFiDirectAdvertisementPublisher,
}

impl Group {
    pub fn is_up(&self) -> bool {
        self.publisher
            .Status()
            .is_ok_and(|s| s == WiFiDirectAdvertisementPublisherStatus::Started)
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Err(e) = self.publisher.Stop() {
            tracing::warn!(error = %e, "could not stop the direct Wi-Fi group");
        }
    }
}

fn current() -> &'static Mutex<Weak<Group>> {
    static GROUP: Mutex<Weak<Group>> = Mutex::new(Weak::new());
    &GROUP
}

pub fn host() -> Result<Arc<Group>> {
    let mut slot = current().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(group) = slot.upgrade().filter(|g| g.is_up()) {
        return Ok(group);
    }
    let (ssid, passphrase) = crate::fresh_credentials();
    let publisher = WiFiDirectAdvertisementPublisher::new().map_err(wfd_err)?;
    let adv = publisher.Advertisement().map_err(wfd_err)?;
    // Autonomous: the group exists before anyone joins, as a network a
    // station can find. Legacy: any WPA2 station may join it with the
    // passphrase, no Wi-Fi Direct negotiation — which is the only way a Linux
    // machine through NetworkManager can (see the crate's notes).
    adv.SetIsAutonomousGroupOwnerEnabled(true)
        .map_err(wfd_err)?;
    let legacy = adv.LegacySettings().map_err(wfd_err)?;
    legacy.SetIsEnabled(true).map_err(wfd_err)?;
    legacy.SetSsid(&HSTRING::from(&ssid)).map_err(wfd_err)?;
    let cred = PasswordCredential::new().map_err(wfd_err)?;
    cred.SetPassword(&HSTRING::from(&passphrase))
        .map_err(wfd_err)?;
    legacy.SetPassphrase(&cred).map_err(wfd_err)?;
    publisher.Start().map_err(wfd_err)?;
    let started = Instant::now();
    loop {
        match publisher.Status().map_err(wfd_err)? {
            WiFiDirectAdvertisementPublisherStatus::Started => break,
            WiFiDirectAdvertisementPublisherStatus::Aborted
            | WiFiDirectAdvertisementPublisherStatus::Stopped => {
                return Err(DirectError::WiFiDirect(
                    "Windows stopped the group as it started; is Wi-Fi on?".into(),
                ))
            }
            _ if started.elapsed() > START_WITHIN => {
                let _ = publisher.Stop();
                return Err(DirectError::WiFiDirect(format!(
                    "the group did not start within {} s",
                    START_WITHIN.as_secs()
                )));
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    tracing::info!(%ssid, "direct Wi-Fi group up");
    let group = Arc::new(Group {
        ssid,
        passphrase,
        publisher,
    });
    *slot = Arc::downgrade(&group);
    Ok(group)
}

pub fn ability() -> Ability {
    match own_channel() {
        Ok(channel) => Ability {
            host: true,
            join: false,
            freq_mhz: channel.and_then(channel_freq_mhz),
            why_not: None,
        },
        Err(e) => Ability {
            why_not: Some(e),
            ..Ability::default()
        },
    }
}

/// The channel of the first Wi-Fi interface that is on a network, or `None`
/// when none is. An error means there is no Wi-Fi to run a group on.
fn own_channel() -> std::result::Result<Option<u32>, String> {
    let mut negotiated = 0u32;
    let mut handle = HANDLE::default();
    // WLAN_API_VERSION_2_0.
    let opened = unsafe { WlanOpenHandle(2, None, &mut negotiated, &mut handle) };
    if opened != 0 {
        return Err(format!("the WLAN service is not running (error {opened})"));
    }
    let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
    let result = (|| {
        let listed = unsafe { WlanEnumInterfaces(handle, None, &mut list) };
        if listed != 0 || list.is_null() {
            return Err(format!("cannot list Wi-Fi interfaces (error {listed})"));
        }
        let count = unsafe { (*list).dwNumberOfItems } as usize;
        if count == 0 {
            return Err("this machine has no Wi-Fi interface".to_string());
        }
        let infos = unsafe { std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count) };
        for info in infos {
            let mut size = 0u32;
            let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
            let asked = unsafe {
                WlanQueryInterface(
                    handle,
                    &info.InterfaceGuid,
                    wlan_intf_opcode_channel_number,
                    None,
                    &mut size,
                    &mut data,
                    None,
                )
            };
            // Not connected: the query fails, and this interface has no
            // channel to compare.
            if asked == 0 && !data.is_null() {
                let channel = unsafe { *(data as *const u32) };
                unsafe { WlanFreeMemory(data) };
                return Ok(Some(channel));
            }
        }
        Ok(None)
    })();
    if !list.is_null() {
        unsafe { WlanFreeMemory(list as *const std::ffi::c_void) };
    }
    unsafe { WlanCloseHandle(handle, None) };
    result
}
