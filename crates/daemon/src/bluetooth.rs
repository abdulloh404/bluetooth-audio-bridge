use crate::config::{config_home, ensure_user, Config};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zbus::zvariant::OwnedValue;

const A2DP_SOURCE: &str = "0000110a-0000-1000-8000-00805f9b34fb";
const A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub address: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub uuids: Vec<String>,
    #[serde(skip)]
    object_path: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BluetoothStatus {
    pub available: bool,
    pub phone: Option<Device>,
    pub headphones: Option<Device>,
    pub phone_reconnect: String,
    pub headphones_reconnect: String,
    pub last_error: String,
}

#[derive(Clone)]
pub struct MonitorConfig {
    pub config: Config,
    pub phone_policy_observed: bool,
}

pub fn phone_policy_file_present(address: &str) -> bool {
    let Ok(home) = config_home() else { return false };
    let Ok(uid) = ensure_user() else { return false };
    let marker = format!("BLUETOOTH_AUDIO_BRIDGE_PHONE={address}");
    ["wireplumber/bluetooth.lua.d/90-bluetooth-audio-bridge-phone.lua", "wireplumber/wireplumber.conf.d/90-bluetooth-audio-bridge-phone.conf"]
        .iter().any(|relative| {
            let path = home.join(relative);
            let Ok(meta) = fs::symlink_metadata(&path) else { return false };
            meta.is_file() && meta.uid() == uid && meta.len() <= 65536 &&
                fs::read_to_string(path).is_ok_and(|text| {
                    let mut lines = text.lines().map(|line| line.trim().trim_start_matches("--").trim_start_matches('#').trim());
                    lines.clone().any(|line| line == marker) && lines.any(|line| line == "BLUETOOTH_AUDIO_BRIDGE_ROUTE=direct")
                })
        })
}

async fn snapshot(connection: &zbus::Connection) -> Result<Vec<Device>> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(connection).destination("org.bluez")?.path("/")?.build().await?;
    let objects = proxy.get_managed_objects().await?;
    let mut devices = Vec::new();
    for (path, interfaces) in objects {
        let Some(mut values) = interfaces.into_iter().find_map(|(name, properties)| (name.as_str() == "org.bluez.Device1").then_some(properties)) else { continue };
        let string = |value: Option<&OwnedValue>| value.and_then(|value| <&str>::try_from(value).ok()).unwrap_or("").to_owned();
        let address = string(values.get("Address")).to_ascii_uppercase();
        if address.is_empty() { continue; }
        let mut name = string(values.get("Alias"));
        if name.is_empty() { name = string(values.get("Name")); }
        let paired = values.get("Paired").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
        let connected = values.get("Connected").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
        let uuids = values.remove("UUIDs").and_then(|v| Vec::<String>::try_from(v).ok()).unwrap_or_default();
        devices.push(Device { address, name, paired, connected, uuids, object_path: path.to_string() });
    }
    devices.sort_by(|left, right| left.address.cmp(&right.address));
    Ok(devices)
}

pub async fn list_devices() -> Result<Vec<Device>> {
    ensure_user()?;
    tokio::time::timeout(DBUS_TIMEOUT, async {
        let connection = zbus::Connection::system().await.context("Cannot connect to the system D-Bus")?;
        snapshot(&connection).await.context("Cannot list BlueZ devices")
    }).await.context("BlueZ device query timed out")?
}

pub async fn validate_selection(phone: &str, headphones: &str) -> Result<()> {
    let devices = list_devices().await?;
    for (address, uuid, role) in [(phone, A2DP_SOURCE, "A2DP Source (phone)"), (headphones, A2DP_SINK, "A2DP Sink (headphones)")] {
        let device = devices.iter().find(|device| device.address.eq_ignore_ascii_case(address))
            .with_context(|| format!("{address} is not known to BlueZ; pair it explicitly using the system Bluetooth UI"))?;
        if !device.paired { bail!("{address} is not paired; pair it explicitly using the system Bluetooth UI"); }
        if !device.uuids.iter().any(|value| value.eq_ignore_ascii_case(uuid)) {
            bail!("{address} does not advertise {role}; check pairing and device capabilities");
        }
    }
    Ok(())
}

struct Retry {
    address: String,
    next: Instant,
    delay: u64,
    pending: Option<JoinHandle<Result<()>>>,
    message: String,
}

impl Drop for Retry {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() { task.abort(); }
    }
}

impl Retry {
    fn new(initial: u64) -> Self {
        Self { address: String::new(), next: Instant::now(), delay: initial, pending: None, message: String::new() }
    }

    async fn update(&mut self, connection: &zbus::Connection, device: Option<&Device>, address: &str, uuid: &'static str, settings: &MonitorConfig, phone: bool) {
        let configuration = &settings.config.connection;
        if self.address != address {
            if let Some(task) = self.pending.take() { task.abort(); }
            self.address = address.into();
            self.delay = configuration.retry_initial_seconds;
            self.next = Instant::now();
            self.message.clear();
        }
        if self.pending.as_ref().is_some_and(|task| task.is_finished()) {
            if let Some(task) = self.pending.take() {
                match task.await {
                    Ok(Ok(())) => self.message = "A2DP connection requested; waiting for BlueZ and audio readiness".into(),
                    Ok(Err(error)) => self.message = format!("A2DP reconnect failed: {error:#}"),
                    Err(error) => self.message = format!("A2DP reconnect cancelled: {error}"),
                }
                self.next = Instant::now() + Duration::from_secs(self.delay);
                self.delay = self.delay.saturating_mul(2).min(configuration.retry_max_seconds);
            }
        }
        if !configuration.auto_reconnect || !settings.config.audio.routing_enabled {
            if let Some(task) = self.pending.take() { task.abort(); }
            self.message = "Automatic reconnect disabled".into();
            return;
        }
        let Some(device) = device else {
            self.message = "Selected device is not known to BlueZ; pair it explicitly".into();
            return;
        };
        if !device.paired {
            self.message = "Selected device is not paired; pair it explicitly".into();
            return;
        }
        if device.connected {
            self.delay = configuration.retry_initial_seconds;
            self.message = "Bluetooth connected; audio readiness is reported separately".into();
            return;
        }
        if phone && (!settings.phone_policy_observed || !phone_policy_file_present(address)) {
            if let Some(task) = self.pending.take() { task.abort(); }
            self.message = "Waiting for loaded direct phone routing policy: install the scoped rule, load it in WirePlumber, then connect the phone explicitly once".into();
            return;
        }
        if !device.uuids.iter().any(|value| value.eq_ignore_ascii_case(uuid)) {
            self.message = "Selected device does not advertise the required A2DP role".into();
            return;
        }
        if self.pending.is_some() || Instant::now() < self.next { return; }
        let connection = connection.clone();
        let path = device.object_path.clone();
        self.message = "Connecting the selected A2DP profile".into();
        self.pending = Some(tokio::spawn(async move {
            tokio::time::timeout(DBUS_TIMEOUT, async {
                let proxy = zbus::Proxy::new(&connection, "org.bluez", path.as_str(), "org.bluez.Device1").await?;
                let _: () = proxy.call("ConnectProfile", &(uuid,)).await?;
                Ok::<(), anyhow::Error>(())
            }).await.context("BlueZ A2DP reconnect timed out")?
        }));
    }
}

pub async fn monitor(settings: watch::Receiver<MonitorConfig>, status: watch::Sender<BluetoothStatus>) {
    let initial = settings.borrow().config.connection.retry_initial_seconds;
    let mut phone_retry = Retry::new(initial);
    let mut headphones_retry = Retry::new(initial);
    let mut connection: Option<zbus::Connection> = None;
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if status.is_closed() { return; }
        let result = tokio::time::timeout(DBUS_TIMEOUT, async {
            if connection.is_none() { connection = Some(zbus::Connection::system().await?); }
            snapshot(connection.as_ref().context("D-Bus connection unavailable")?).await
        }).await;
        let devices = match result {
            Ok(Ok(devices)) => devices,
            other => {
                connection = None;
                let message = match other { Ok(Err(error)) => format!("BlueZ unavailable: {error:#}"), _ => "BlueZ snapshot timed out".into() };
                status.send_replace(BluetoothStatus { last_error: message, ..Default::default() });
                continue;
            }
        };
        let settings = settings.borrow().clone();
        let phone = devices.iter().find(|device| device.address == settings.config.devices.iphone_address).cloned();
        let headphones = devices.iter().find(|device| device.address == settings.config.devices.headphones_address).cloned();
        if let Some(connection) = &connection {
            phone_retry.update(connection, phone.as_ref(), &settings.config.devices.iphone_address, A2DP_SOURCE, &settings, true).await;
            headphones_retry.update(connection, headphones.as_ref(), &settings.config.devices.headphones_address, A2DP_SINK, &settings, false).await;
        }
        status.send_replace(BluetoothStatus {
            available: true, phone, headphones,
            phone_reconnect: phone_retry.message.clone(),
            headphones_reconnect: headphones_retry.message.clone(),
            last_error: String::new(),
        });
    }
}
