//! อ่านสถานะจาก BlueZ โดยให้ Ubuntu จัดการการเชื่อมต่อ Bluetooth
use crate::config::{config_home, ensure_user};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;
use tokio::sync::watch;
use zbus::zvariant::OwnedValue;

const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub address: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub uuids: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BluetoothStatus {
    pub available: bool,
    pub devices: Vec<Device>,
    pub last_error: String,
}

pub fn phone_policy_file_present() -> bool {
    let Ok(home) = config_home() else { return false };
    let Ok(uid) = ensure_user() else { return false };
    let required = ["BLUETOOTH_AUDIO_BRIDGE_POLICY=1", "BLUETOOTH_AUDIO_BRIDGE_SCOPE=a2dp-source", "BLUETOOTH_AUDIO_BRIDGE_ROUTE=system-output"];
    ["wireplumber/bluetooth.lua.d/90-bluetooth-audio-bridge-phone.lua", "wireplumber/wireplumber.conf.d/90-bluetooth-audio-bridge-phone.conf"]
        .iter().any(|relative| {
            let path = home.join(relative);
            let Ok(meta) = fs::symlink_metadata(&path) else { return false };
            meta.is_file() && meta.uid() == uid && meta.len() <= 65536 &&
                fs::read_to_string(path).is_ok_and(|text| {
                    required.iter().all(|marker| text.lines()
                        .map(|line| line.trim().trim_start_matches("--").trim_start_matches('#').trim())
                        .any(|line| line == *marker))
                })
        })
}

async fn snapshot(connection: &zbus::Connection) -> Result<Vec<Device>> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(connection).destination("org.bluez")?.path("/")?.build().await?;
    let objects = proxy.get_managed_objects().await?;
    let mut devices = Vec::new();
    for (_, interfaces) in objects {
        let Some(mut values) = interfaces.into_iter().find_map(|(name, properties)| (name.as_str() == "org.bluez.Device1").then_some(properties)) else { continue };
        let string = |value: Option<&OwnedValue>| value.and_then(|value| <&str>::try_from(value).ok()).unwrap_or("").to_owned();
        let address = string(values.get("Address")).to_ascii_uppercase();
        if address.is_empty() { continue; }
        let mut name = string(values.get("Alias"));
        if name.is_empty() { name = string(values.get("Name")); }
        let paired = values.get("Paired").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
        let connected = values.get("Connected").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
        let uuids = values.remove("UUIDs").and_then(|v| Vec::<String>::try_from(v).ok()).unwrap_or_default();
        devices.push(Device { address, name, paired, connected, uuids });
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

pub async fn monitor(status: watch::Sender<BluetoothStatus>) {
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
        status.send_replace(BluetoothStatus { available: true, devices, last_error: String::new() });
    }
}
