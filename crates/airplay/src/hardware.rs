//! GPU discovery and profile suitability checks.

use crate::{Resolution, VideoProfile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Other(u32),
}

impl GpuVendor {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn from_pci_id(id: u32) -> Self {
        match id {
            0x10de => Self::Nvidia,
            0x1002 | 0x1022 => Self::Amd,
            0x8086 => Self::Intel,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: GpuVendor,
    /// Dedicated VRAM. Integrated GPUs may legitimately report `None`.
    pub dedicated_vram_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HardwareInfo {
    pub gpus: Vec<GpuInfo>,
}

impl HardwareInfo {
    pub fn detect() -> Self {
        Self {
            gpus: platform_gpus(),
        }
    }

    pub fn profile_warnings(&self, profile: VideoProfile) -> Vec<String> {
        let required_vram = match profile.resolution {
            Resolution::FullHd if profile.fps > 60 => Some(2 * 1024_u64.pow(3)),
            Resolution::Qhd => Some(4 * 1024_u64.pow(3)),
            Resolution::FullHd => None,
        };
        let mut warnings = Vec::new();
        if self.gpus.is_empty() {
            warnings.push(
                "no supported GPU was detected; hardware decode and zero-copy output may fail"
                    .to_string(),
            );
            return warnings;
        }

        if let Some(required) = required_vram {
            let known_max = self
                .gpus
                .iter()
                .filter_map(|gpu| gpu.dedicated_vram_bytes)
                .max();
            if known_max.is_some_and(|available| available < required) {
                warnings.push(format!(
                    "selected profile expects at least {} GiB dedicated VRAM, detected {:.1} GiB",
                    required / 1024_u64.pow(3),
                    known_max.unwrap_or_default() as f64 / 1024_f64.powi(3)
                ));
            } else if known_max.is_none()
                && self.gpus.iter().all(|gpu| gpu.vendor == GpuVendor::Intel)
            {
                warnings.push(
                    "selected profile is demanding and only shared-memory Intel graphics was detected"
                        .to_string(),
                );
            }
        }
        warnings
    }
}

#[cfg(target_os = "linux")]
fn platform_gpus() -> Vec<GpuInfo> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut gpus = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Some(index) = name.strip_prefix("card") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let device = entry.path().join("device");
        let Some(vendor_id) = read_hex_u32(&device.join("vendor")) else {
            continue;
        };
        let device_id = read_hex_u32(&device.join("device"));
        let vendor = GpuVendor::from_pci_id(vendor_id);
        let dedicated_vram_bytes = read_decimal_u64(&device.join("mem_info_vram_total"));
        let label = match vendor {
            GpuVendor::Nvidia => "NVIDIA",
            GpuVendor::Amd => "AMD",
            GpuVendor::Intel => "Intel",
            GpuVendor::Other(_) => "GPU",
        };
        let name = device_id.map_or_else(
            || format!("{label} {name}"),
            |id| format!("{label} {name} (PCI {id:04x})"),
        );
        gpus.push(GpuInfo {
            name,
            vendor,
            dedicated_vram_bytes,
        });
    }
    gpus
}

#[cfg(target_os = "linux")]
fn read_hex_u32(path: &std::path::Path) -> Option<u32> {
    let value = std::fs::read_to_string(path).ok()?;
    u32::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()
}

#[cfg(target_os = "linux")]
fn read_decimal_u64(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(target_os = "windows")]
fn platform_gpus() -> Vec<GpuInfo> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    let mut gpus = Vec::new();
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let Ok(description) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let name_len = description
            .Description
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(description.Description.len());
        gpus.push(GpuInfo {
            name: String::from_utf16_lossy(&description.Description[..name_len]),
            vendor: GpuVendor::from_pci_id(description.VendorId),
            dedicated_vram_bytes: Some(description.DedicatedVideoMemory as u64),
        });
    }
    gpus
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_gpus() -> Vec<GpuInfo> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_gpu_warns_for_qhd() {
        let hardware = HardwareInfo {
            gpus: vec![GpuInfo {
                name: "small".into(),
                vendor: GpuVendor::Amd,
                dedicated_vram_bytes: Some(1024_u64.pow(3)),
            }],
        };
        let profile = VideoProfile::new(Resolution::Qhd, 60).unwrap();
        assert_eq!(hardware.profile_warnings(profile).len(), 1);
    }

    #[test]
    fn ordinary_full_hd_has_no_vram_warning() {
        let hardware = HardwareInfo {
            gpus: vec![GpuInfo {
                name: "integrated".into(),
                vendor: GpuVendor::Intel,
                dedicated_vram_bytes: None,
            }],
        };
        let profile = VideoProfile::new(Resolution::FullHd, 60).unwrap();
        assert!(hardware.profile_warnings(profile).is_empty());
    }
}
