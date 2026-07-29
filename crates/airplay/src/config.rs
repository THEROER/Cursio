use std::fmt;
use std::net::SocketAddr;

use uuid::Uuid;

/// Default port used by Apple TV receivers for the AirPlay control connection.
pub const DEFAULT_CONTROL_PORT: u16 = 7000;
/// Compatibility model advertised during the initial AirPlay probe.
pub const DEFAULT_MODEL: &str = "AppleTV2,1";
/// AirTunes source version advertised to clients.
pub const DEFAULT_SOURCE_VERSION: &str = "220.68";

/// Stable six-byte identity shared by the AirPlay and RAOP announcements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId([u8; 6]);

impl DeviceId {
    /// Derive a stable, locally administered unicast MAC-like id from a UUID.
    ///
    /// It is not claimed to be a physical interface address. Using the node's
    /// persisted UUID keeps the advertised receiver identity stable across
    /// restarts without colliding with a real NIC.
    pub fn from_uuid(id: Uuid) -> Self {
        let mut bytes = [0; 6];
        bytes.copy_from_slice(&id.as_bytes()[..6]);
        bytes[0] = (bytes[0] | 0x02) & 0xfe;
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }

    /// RAOP instance prefix (`AABBCCDDEEFF`, without separators).
    pub fn raop_prefix(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02X}")).collect()
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// Resolution requested from the sender and used to configure local output.
///
/// AirPlay clients remain free to select a different encoded stream size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    FullHd,
    Qhd,
}

impl Resolution {
    pub const fn dimensions(self) -> (u16, u16) {
        match self {
            Self::FullHd => (1920, 1080),
            Self::Qhd => (2560, 1440),
        }
    }
}

/// Requested video profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoProfile {
    pub resolution: Resolution,
    pub fps: u16,
}

impl VideoProfile {
    pub fn new(resolution: Resolution, fps: u16) -> Result<Self, ConfigError> {
        if !(1..=120).contains(&fps) {
            return Err(ConfigError::FpsOutOfRange(fps));
        }
        Ok(Self { resolution, fps })
    }
}

/// Receiver identity and control-listener settings.
#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub name: String,
    pub identity: Uuid,
    pub device_id: DeviceId,
    pub bind_addr: SocketAddr,
    pub model: String,
    pub source_version: String,
    pub profile: VideoProfile,
}

impl ReceiverConfig {
    pub fn new(
        name: impl Into<String>,
        identity: Uuid,
        bind_addr: SocketAddr,
        profile: VideoProfile,
    ) -> Result<Self, ConfigError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(ConfigError::EmptyName);
        }
        Ok(Self {
            name,
            identity,
            device_id: DeviceId::from_uuid(identity),
            bind_addr,
            model: DEFAULT_MODEL.to_string(),
            source_version: DEFAULT_SOURCE_VERSION.to_string(),
            profile,
        })
    }

    pub fn with_default_port(
        name: impl Into<String>,
        identity: Uuid,
        profile: VideoProfile,
    ) -> Result<Self, ConfigError> {
        Self::new(
            name,
            identity,
            SocketAddr::from(([0, 0, 0, 0], DEFAULT_CONTROL_PORT)),
            profile,
        )
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("FPS must be in 1..=120, got {0}")]
    FpsOutOfRange(u16),
    #[error("receiver name must not be empty")]
    EmptyName,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_local_and_unicast() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let first = DeviceId::from_uuid(id);
        let second = DeviceId::from_uuid(id);
        assert_eq!(first, second);
        assert_eq!(first.as_bytes()[0] & 0x02, 0x02);
        assert_eq!(first.as_bytes()[0] & 0x01, 0);
        assert_eq!(first.to_string(), "02:11:22:33:44:55");
        assert_eq!(first.raop_prefix(), "021122334455");
    }

    #[test]
    fn resolution_dimensions_are_exact() {
        assert_eq!(Resolution::FullHd.dimensions(), (1920, 1080));
        assert_eq!(Resolution::Qhd.dimensions(), (2560, 1440));
    }

    #[test]
    fn fps_range_is_bounded() {
        assert!(VideoProfile::new(Resolution::FullHd, 1).is_ok());
        assert!(VideoProfile::new(Resolution::Qhd, 120).is_ok());
        assert_eq!(
            VideoProfile::new(Resolution::FullHd, 0).unwrap_err(),
            ConfigError::FpsOutOfRange(0)
        );
        assert_eq!(
            VideoProfile::new(Resolution::FullHd, 121).unwrap_err(),
            ConfigError::FpsOutOfRange(121)
        );
    }

    #[test]
    fn receiver_name_must_not_be_blank() {
        let profile = VideoProfile::new(Resolution::FullHd, 60).unwrap();
        let error = ReceiverConfig::with_default_port("  ", Uuid::nil(), profile).unwrap_err();
        assert_eq!(error, ConfigError::EmptyName);
    }
}
