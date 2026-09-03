use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub devices: Devices,
    pub audio: Audio,
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
    pub virtual_sink_name: String,
    pub output_codec: String,
    pub allow_codec_fallback: bool,
    pub phone_gain: f32,
    pub desktop_gain: f32,
    pub master_gain: f32,
    pub phone_mute: bool,
    pub desktop_mute: bool,
    pub master_mute: bool,
    pub routing_enabled: bool,
    pub headphone_disconnect_action: String,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            virtual_sink_name: "bluetooth-audio-bridge".into(),
            output_codec: "aac".into(),
            allow_codec_fallback: false,
            phone_gain: 0.5,
            desktop_gain: 0.5,
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

pub fn normalize_address(address: &str) -> Result<String> {
    if address.len() != 17 || !address.split(':').all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_hexdigit())) {
        bail!("Invalid Bluetooth address: {address}");
    }
    Ok(address.to_ascii_uppercase())
}

pub fn validate_gain(value: f32) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("Gain must be a finite number between 0 and 1");
    }
    Ok(())
}

impl Config {
    pub fn validate(&self, require_devices: bool) -> Result<()> {
        let phone = &self.devices.iphone_address;
        let headphones = &self.devices.headphones_address;
        for address in [phone, headphones] {
            if !address.is_empty() { normalize_address(address)?; }
        }
        if require_devices && (phone.is_empty() || headphones.is_empty()) {
            bail!("Select both paired devices with: bluetooth-audio-bridge select --iphone MAC --headphones MAC");
        }
        if !phone.is_empty() && phone.eq_ignore_ascii_case(headphones) {
            bail!("iPhone and headphones must be distinct devices");
        }
        for gain in [self.audio.phone_gain, self.audio.desktop_gain, self.audio.master_gain] {
            validate_gain(gain)?;
        }
        if self.audio.output_codec != "aac" || self.audio.headphone_disconnect_action != "silence" {
            bail!("Only output_codec = \"aac\" and headphone_disconnect_action = \"silence\" are supported");
        }
        let name = &self.audio.virtual_sink_name;
        if name.is_empty() || name.len() > 100 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
            bail!("virtual_sink_name must contain 1-100 ASCII letters, digits, '-', '_' or '.'");
        }
        if self.connection.retry_initial_seconds == 0 || self.connection.retry_initial_seconds > self.connection.retry_max_seconds || self.connection.retry_max_seconds > 300 {
            bail!("Reconnect delays must satisfy 1 <= retry_initial_seconds <= retry_max_seconds <= 300");
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
        let mut config: Self = toml::from_str(&content).context("Invalid TOML configuration")?;
        config.validate(false)?;
        if !config.devices.iphone_address.is_empty() {
            config.devices.iphone_address = normalize_address(&config.devices.iphone_address)?;
        }
        if !config.devices.headphones_address.is_empty() {
            config.devices.headphones_address = normalize_address(&config.devices.headphones_address)?;
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate(false)?;
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
