//! อ่านสถานะ BlueZ และเปิด A2DP สำหรับแหล่งเสียงที่เชื่อมอยู่แล้วแต่ยังไม่มี transport
use crate::config::{config_home, ensure_user};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use zbus::zvariant::{ObjectPath, OwnedValue};

const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
const A2DP_SOURCE_UUID: &str = "0000110a-0000-1000-8000-00805f9b34fb";
const A2DP_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const A2DP_CONNECT_DELAY: Duration = Duration::from_secs(2);
const A2DP_RETRY_DELAY: Duration = Duration::from_secs(30);

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

struct Snapshot {
    devices: Vec<Device>,
    missing_sources: Vec<(String, String)>,
}

struct A2dpRetry {
    next_attempt: Instant,
    last_error: String,
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

async fn snapshot(connection: &zbus::Connection) -> Result<Snapshot> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(connection).destination("org.bluez")?.path("/")?.build().await?;
    let objects = proxy.get_managed_objects().await?;
    // transport ฝั่งรับใช้ UUID ของ local A2DP Sink และยังมีอยู่ได้ขณะโทรศัพท์พักเพลง
    let transports: HashSet<String> = objects.values().filter_map(|interfaces| {
        let properties = interfaces.iter().find_map(|(name, properties)| (name.as_str() == "org.bluez.MediaTransport1").then_some(properties))?;
        let uuid = <&str>::try_from(properties.get("UUID")?).ok()?;
        if !uuid.eq_ignore_ascii_case(A2DP_SINK_UUID) { return None; }
        let path = <&ObjectPath<'_>>::try_from(properties.get("Device")?).ok()?;
        Some(path.as_str().to_owned())
    }).collect();
    let mut devices = Vec::new();
    let mut missing_sources = Vec::new();
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
        if paired && connected && uuids.iter().any(|uuid| uuid.eq_ignore_ascii_case(A2DP_SOURCE_UUID)) && !transports.contains(path.as_str()) {
            missing_sources.push((path.to_string(), address.clone()));
        }
        devices.push(Device { address, name, paired, connected, uuids });
    }
    devices.sort_by(|left, right| left.address.cmp(&right.address));
    missing_sources.sort();
    Ok(Snapshot { devices, missing_sources })
}

pub async fn list_devices() -> Result<Vec<Device>> {
    ensure_user()?;
    tokio::time::timeout(DBUS_TIMEOUT, async {
        let connection = zbus::Connection::system().await.context("Cannot connect to the system D-Bus")?;
        snapshot(&connection).await.map(|snapshot| snapshot.devices).context("Cannot list BlueZ devices")
    }).await.context("BlueZ device query timed out")?
}

async fn connect_missing_sources(connection: &zbus::Connection, sources: &[(String, String)], enabled: &watch::Receiver<bool>, retries: &mut HashMap<String, A2dpRetry>) -> String {
    if !*enabled.borrow() {
        retries.clear();
        return String::new();
    }
    retries.retain(|path, _| sources.iter().any(|(source, _)| source == path));
    for (path, address) in sources {
        if !*enabled.borrow() { break; }
        // รอให้การเชื่อมจากโทรศัพท์เสร็จก่อน และเว้นช่วงเพื่อไม่รบกวน BlueZ เมื่อเชื่อมไม่สำเร็จ
        let retry = retries.entry(path.clone()).or_insert_with(|| A2dpRetry {
            next_attempt: Instant::now() + A2DP_CONNECT_DELAY,
            last_error: String::new(),
        });
        if Instant::now() < retry.next_attempt { continue; }
        let result = tokio::time::timeout(DBUS_TIMEOUT, async {
            let proxy = zbus::Proxy::new(connection, "org.bluez", path.as_str(), "org.bluez.Device1").await?;
            if !proxy.get_property::<bool>("Paired").await? || !proxy.get_property::<bool>("Connected").await? || !*enabled.borrow() {
                return Ok::<(), zbus::Error>(());
            }
            // ConnectProfile ระบุ BR/EDR โดยตรง จึงเปิด A2DP ได้แม้ Connect ปกติเลือก LE
            proxy.call::<_, _, ()>("ConnectProfile", &(A2DP_SOURCE_UUID,)).await
        }).await;
        retry.next_attempt = Instant::now() + A2DP_RETRY_DELAY;
        let error = match result {
            Ok(Ok(())) => String::new(),
            Ok(Err(error)) => format!("Cannot connect incoming A2DP for {address}: {error}"),
            Err(_) => format!("Incoming A2DP connection timed out for {address}"),
        };
        if !error.is_empty() && retry.last_error != error {
            eprintln!("{}", serde_json::json!({ "event": "bluetooth_a2dp_connect_error", "message": error }));
        }
        retry.last_error = error;
    }
    sources.iter().filter_map(|(path, _)| retries.get(path)).map(|retry| retry.last_error.as_str())
        .filter(|error| !error.is_empty()).collect::<Vec<_>>().join("; ")
}

pub async fn monitor(status: watch::Sender<BluetoothStatus>, enabled: watch::Receiver<bool>) {
    let mut connection: Option<zbus::Connection> = None;
    let mut retries = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if status.is_closed() { return; }
        let result = tokio::time::timeout(DBUS_TIMEOUT, async {
            if connection.is_none() { connection = Some(zbus::Connection::system().await?); }
            snapshot(connection.as_ref().context("D-Bus connection unavailable")?).await
        }).await;
        let snapshot = match result {
            Ok(Ok(snapshot)) => snapshot,
            other => {
                connection = None;
                let message = match other { Ok(Err(error)) => format!("BlueZ unavailable: {error:#}"), _ => "BlueZ snapshot timed out".into() };
                status.send_replace(BluetoothStatus { last_error: message, ..Default::default() });
                continue;
            }
        };
        let last_error = if let Some(connection) = connection.as_ref() {
            connect_missing_sources(connection, &snapshot.missing_sources, &enabled, &mut retries).await
        } else { String::new() };
        status.send_replace(BluetoothStatus { available: true, devices: snapshot.devices, last_error });
    }
}
