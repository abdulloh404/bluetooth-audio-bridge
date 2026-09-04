use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing)]
    pub devices: Devices,
    pub audio: Audio,
    pub bluetooth: Bluetooth,
    #[serde(skip_serializing)]
    pub connection: Connection,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Devices {
    pub iphone_address: String,
    pub headphones_address: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    #[serde(skip_serializing)]
    pub virtual_sink_name: String,
    #[serde(skip_serializing)]
    pub output_codec: String,
    #[serde(skip_serializing)]
    pub allow_codec_fallback: bool,
    pub phone_gain: f32,
    pub desktop_gain: f32,
    pub master_gain: f32,
    pub phone_mute: bool,
    pub desktop_mute: bool,
    pub master_mute: bool,
    pub routing_enabled: bool,
    #[serde(skip_serializing)]
    pub headphone_disconnect_action: String,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            virtual_sink_name: "bluetooth-audio-bridge".into(),
            output_codec: "aac".into(),
            allow_codec_fallback: false,
            phone_gain: 1.0,
            desktop_gain: 1.0,
            master_gain: 1.0,
            phone_mute: false,
            desktop_mute: false,
            master_mute: false,
            routing_enabled: true,
            headphone_disconnect_action: "silence".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bluetooth {
    pub dbus_timeout_seconds: u64,
    pub a2dp_connect_delay_seconds: u64,
    pub a2dp_retry_delay_seconds: u64,
}

impl Default for Bluetooth {
    fn default() -> Self {
        // ใช้ไฟล์เดียวกับ installer เป็นค่าเริ่มต้น เพื่อไม่กำหนดเวลาไว้ซ้ำในโค้ด
        let defaults: toml::Table = toml::from_str(include_str!("../../../config/default.toml"))
            .expect("Bundled default configuration must be valid TOML");
        let seconds = |key: &str| defaults.get("bluetooth").and_then(|table| table.get(key))
            .and_then(toml::Value::as_integer).and_then(|value| u64::try_from(value).ok())
            .expect("Bundled Bluetooth timing defaults must be nonnegative integers");
        Self {
            dbus_timeout_seconds: seconds("dbus_timeout_seconds"),
            a2dp_connect_delay_seconds: seconds("a2dp_connect_delay_seconds"),
            a2dp_retry_delay_seconds: seconds("a2dp_retry_delay_seconds"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Connection {
    pub auto_reconnect: bool,
    pub retry_initial_seconds: u64,
    pub retry_max_seconds: u64,
}

impl Default for Connection {
    fn default() -> Self {
        Self { auto_reconnect: true, retry_initial_seconds: 1, retry_max_seconds: 30 }
    }
}

pub fn ensure_user() -> Result<u32> {
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        bail!("Run Bluetooth Audio Bridge as the desktop user, not root");
    }
    Ok(uid)
}

pub fn config_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("XDG_CONFIG_HOME must be an absolute path");
        }
        return Ok(path);
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    if !home.is_absolute() {
        bail!("HOME must be an absolute path");
    }
    Ok(home.join(".config"))
}

pub fn config_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) if path.is_absolute() => path,
        Some(path) => std::env::current_dir()?.join(path),
        None => config_home()?.join("bluetooth-audio-bridge/config.toml"),
    };
    if path.file_name().is_none() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        bail!("Config path must name a file and must not contain '..'");
    }
    Ok(path)
}

pub fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_dir() || meta.uid() != ensure_user()? || meta.mode() & 0o077 != 0 {
                bail!("{} must be a user-owned directory with mode 0700", path.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
                }
            }
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return private_dir(path),
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn validate_gain(value: f32) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("Gain must be a finite number between 0 and 1");
    }
    Ok(())
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        for gain in [self.audio.phone_gain, self.audio.desktop_gain, self.audio.master_gain] {
            validate_gain(gain)?;
        }
        if self.bluetooth.dbus_timeout_seconds == 0 || self.bluetooth.a2dp_retry_delay_seconds == 0 {
            bail!("bluetooth.dbus_timeout_seconds and bluetooth.a2dp_retry_delay_seconds must be greater than zero");
        }
        for (name, seconds) in [
            ("dbus_timeout_seconds", self.bluetooth.dbus_timeout_seconds),
            ("a2dp_connect_delay_seconds", self.bluetooth.a2dp_connect_delay_seconds),
            ("a2dp_retry_delay_seconds", self.bluetooth.a2dp_retry_delay_seconds),
        ] {
            if Instant::now().checked_add(Duration::from_secs(seconds)).is_none() {
                bail!("bluetooth.{name} exceeds the supported timer duration");
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)
            .with_context(|| format!("Cannot read {}; run 'bluetooth-audio-bridge config init' first", path.display()))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != ensure_user()? || metadata.mode() & 0o077 != 0 {
            bail!("{} must be a user-owned regular file with mode 0600", path.display());
        }
        if metadata.len() > 65536 { bail!("Config is larger than 64 KiB"); }
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        let config: Self = toml::from_str(&content).context("Invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path.parent().context("Config path has no parent")?;
        private_dir(parent)?;
        if let Ok(meta) = fs::symlink_metadata(path) {
            if !meta.is_file() || meta.uid() != ensure_user()? {
                bail!("Refusing to replace a symlink or a file owned by another user");
            }
        }
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let temporary = parent.join(format!(".config-{}-{}.tmp", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;
        let result = (|| -> Result<()> {
            file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() { let _ = fs::remove_file(&temporary); }
        result
    }
}
