use crate::config::{ensure_user, private_dir, Config};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Semaphore};

const MAX_MESSAGE: usize = 65536;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel { Phone, Desktop, Master }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status,
    ConfigShow,
    Select { iphone_address: String, headphones_address: String },
    Volume { channel: Channel, value: f32 },
    Mute { channel: Channel, muted: bool },
    Enable { enabled: bool },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub config_path: PathBuf,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn success(message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        Self { ok: true, message: message.into(), data }
    }

    pub fn error(error: impl std::fmt::Display) -> Self {
        Self { ok: false, message: error.to_string(), data: None }
    }
}

pub fn apply_command(config: &mut Config, command: &Command) -> Result<()> {
    match command {
        Command::Select { iphone_address, headphones_address } => {
            config.devices.iphone_address = crate::config::normalize_address(iphone_address)?;
            config.devices.headphones_address = crate::config::normalize_address(headphones_address)?;
        }
        Command::Volume { channel, value } => {
            crate::config::validate_gain(*value)?;
            match channel {
                Channel::Phone => config.audio.phone_gain = *value,
                Channel::Desktop => config.audio.desktop_gain = *value,
                Channel::Master => config.audio.master_gain = *value,
            }
        }
        Command::Mute { channel, muted } => match channel {
            Channel::Phone => config.audio.phone_mute = *muted,
            Channel::Desktop => config.audio.desktop_mute = *muted,
            Channel::Master => config.audio.master_mute = *muted,
        },
        Command::Enable { enabled } => config.audio.routing_enabled = *enabled,
        Command::Status | Command::ConfigShow => bail!("This command does not change configuration"),
    }
    config.validate(false)
}

pub fn runtime_dir() -> Result<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set; use a desktop user session")?);
    if !runtime.is_absolute() { bail!("XDG_RUNTIME_DIR must be absolute"); }
    private_dir(&runtime)?;
    let directory = runtime.join("bt-audio-bridge");
    private_dir(&directory)?;
    Ok(directory)
}

pub struct ControllerLock { _file: File }

impl ControllerLock {
    pub fn acquire() -> Result<Self> {
        let path = runtime_dir()?.join("controller.lock");
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW).open(&path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != ensure_user()? || meta.mode() & 0o077 != 0 {
            bail!("Controller lock must be a user-owned regular file with mode 0600");
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("Another controller is running or updating configuration; retry the command");
        }
        // uninstall อาจลบ lock ระหว่าง open กับ flock จึงต้องยืนยันว่า inode ยังตรงกับ path
        let current = fs::symlink_metadata(&path).context("Controller lock was removed during cleanup; retry the command")?;
        if !current.is_file() || current.dev() != meta.dev() || current.ino() != meta.ino() {
            bail!("Controller lock changed during cleanup; retry the command");
        }
        Ok(Self { _file: file })
    }
}

pub struct SocketGuard { path: PathBuf, inode: u64, device: u64 }

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path) {
            if meta.file_type().is_socket() && meta.ino() == self.inode && meta.dev() == self.device {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

pub async fn bind() -> Result<(UnixListener, SocketGuard)> {
    let path = runtime_dir()?.join("control.sock");
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() || meta.uid() != ensure_user()? {
                bail!("Refusing to replace unsafe control socket {}", path.display());
            }
            match tokio::time::timeout(Duration::from_millis(300), UnixStream::connect(&path)).await {
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => fs::remove_file(&path)?,
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => (),
                _ => bail!("Control socket is already in use; it has not been removed"),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let meta = fs::symlink_metadata(&path)?;
    Ok((listener, SocketGuard { path, inode: meta.ino(), device: meta.dev() }))
}

pub struct PendingRequest {
    pub request: Request,
    pub response: oneshot::Sender<Response>,
}

async fn read_message(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    BufReader::new(stream.take((MAX_MESSAGE + 1) as u64)).read_until(b'\n', &mut bytes).await?;
    if bytes.len() > MAX_MESSAGE || bytes.last() != Some(&b'\n') {
        bail!("Expected newline-terminated JSON no larger than 64 KiB");
    }
    Ok(bytes)
}

pub async fn serve(listener: UnixListener, requests: mpsc::Sender<PendingRequest>) -> Result<()> {
    let permits = Arc::new(Semaphore::new(16));
    let uid = ensure_user()?;
    loop {
        let (mut stream, _) = listener.accept().await?;
        if stream.peer_cred()?.uid() != uid { continue; }
        let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
        let requests = requests.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, async {
                let response = async {
                    let bytes = read_message(&mut stream).await?;
                    let request = serde_json::from_slice(&bytes).context("Invalid control request")?;
                    let (response, receiver) = oneshot::channel();
                    requests.send(PendingRequest { request, response }).await.context("Controller stopped")?;
                    receiver.await.context("Controller stopped")
                }.await.unwrap_or_else(Response::error);
                let mut bytes = serde_json::to_vec(&response)?;
                if bytes.len() >= MAX_MESSAGE { bail!("Response is too large"); }
                bytes.push(b'\n');
                stream.write_all(&bytes).await?;
                Ok::<(), anyhow::Error>(())
            }).await;
        });
    }
}

pub async fn request(request: &Request) -> Result<Option<Response>> {
    let path = runtime_dir()?.join("control.sock");
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() || meta.uid() != ensure_user()? || meta.mode() & 0o077 != 0 {
                bail!("Refusing an unsafe control socket");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut stream = match UnixStream::connect(path).await {
            Ok(stream) => stream,
            Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if stream.peer_cred()?.uid() != ensure_user()? { bail!("Control socket belongs to another user"); }
        let mut bytes = serde_json::to_vec(request)?;
        if bytes.len() >= MAX_MESSAGE { bail!("Request is too large"); }
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        let bytes = read_message(&mut stream).await?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }).await.context("Controller request timed out; command outcome is unknown, query status before retrying")?
}
