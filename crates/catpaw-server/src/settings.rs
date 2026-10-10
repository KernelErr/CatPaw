//! The user's settings that an agent may ask to change: where saved
//! downloads go, and the address the local pages (hand-off and approval)
//! listen on. They are kept in `settings.json` in CatPaw's data
//! directory, next to the approval key; command-line flags win over the
//! file for one session. A change asked for with the `settings` tool is
//! made once the user approves it, and saved for later sessions.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Settings, each unset until given (the defaults then apply).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// Where saved downloads go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<PathBuf>,
    /// The address the local pages listen on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_address: Option<IpAddr>,
}

impl Settings {
    /// The settings in `path`; none when it does not exist.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Writes the settings to `path`, its directory made when missing.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text + "\n")
    }

    /// These settings, with what `over` sets in their place.
    pub fn with(&self, over: &Settings) -> Settings {
        Settings {
            download_dir: over
                .download_dir
                .clone()
                .or_else(|| self.download_dir.clone()),
            local_address: over.local_address.or(self.local_address),
        }
    }

    /// The address the local pages listen on: the setting, else
    /// 127.0.0.1.
    pub fn local_address(&self) -> IpAddr {
        self.local_address
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
    }
}

/// `settings.json` in CatPaw's data directory.
pub fn default_file() -> Option<PathBuf> {
    crate::confirm::data_dir().map(|dir| dir.join("settings.json"))
}

/// `CatPaw` in the user's downloads directory (`Downloads` in their home).
pub fn default_download_dir() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)?;
    Some(home.join("Downloads").join("CatPaw"))
}

/// A download directory as given: absolute, so that it means the same
/// whatever the working directory.
pub fn parse_download_dir(text: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(text.trim());
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(format!(
            "downloadDir must be an absolute path, not {text:?}"
        ));
    }
    Ok(path)
}

/// A local address as given: one IP address of this machine to listen on
/// (not 0.0.0.0 or ::, which name none to give the user).
pub fn parse_local_address(text: &str) -> Result<IpAddr, String> {
    let address: IpAddr = text
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| format!("localAddress must be an IP address, not {text:?}"))?;
    if address.is_unspecified() || address.is_multicast() {
        return Err(format!(
            "localAddress must be one address of this machine (such as its LAN address), not {address}"
        ));
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_flags_win() {
        let dir = std::env::temp_dir().join(format!("catpaw-settings-{}", std::process::id()));
        let file = dir.join("settings.json");
        assert_eq!(Settings::load(&file).unwrap(), Settings::default());
        let saved = Settings {
            download_dir: Some(PathBuf::from("/srv/downloads")),
            local_address: Some("192.168.1.20".parse().unwrap()),
        };
        saved.save(&file).unwrap();
        assert_eq!(Settings::load(&file).unwrap(), saved);
        let flags = Settings {
            local_address: Some("10.0.0.2".parse().unwrap()),
            ..Settings::default()
        };
        let effective = saved.with(&flags);
        assert_eq!(
            effective.local_address(),
            "10.0.0.2".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            effective.download_dir,
            Some(PathBuf::from("/srv/downloads"))
        );
        std::fs::write(&file, "{\"what\": 1}").unwrap();
        assert!(Settings::load(&file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn values_are_checked() {
        assert!(parse_local_address("192.168.1.20").is_ok());
        assert!(parse_local_address("[fe80::1]").is_ok());
        assert!(parse_local_address("0.0.0.0").is_err());
        assert!(parse_local_address("nas.local").is_err());
        assert!(parse_download_dir("downloads").is_err());
        let absolute = if cfg!(windows) {
            "D:\\Downloads"
        } else {
            "/srv/downloads"
        };
        assert!(parse_download_dir(absolute).is_ok());
    }
}
