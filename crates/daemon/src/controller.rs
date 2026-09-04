use crate::bluetooth::{self, BluetoothStatus};
use crate::config::{ensure_user, Config};
use crate::ipc::{self, Command, Response};
use anyhow::{Context, Result};
use bluetooth_audio_bridge_audio::{Engine, Levels};
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[derive(Default, Serialize)]
struct AudioStatus {
    pipewire_connected: bool,
    routing_enabled: bool,
    policy_ready: bool,
    inputs_detected: u32,
    inputs_routed: u32,
    default_output_name: String,
    last_error: String,
    routes: Vec<RouteStatus>,
}

#[derive(Serialize)]
struct RouteStatus {
    input_name: String,
    input_address: String,
    output_name: String,
    ready: bool,
    codec: String,
    sample_rate: u32,
    channels: u32,
    last_error: String,
}

impl From<bluetooth_audio_bridge_audio::Status> for AudioStatus {
    fn from(value: bluetooth_audio_bridge_audio::Status) -> Self {
        Self {
            pipewire_connected: value.pipewire_connected,
            routing_enabled: value.routing_enabled,
            policy_ready: value.policy_ready,
            inputs_detected: value.inputs_detected,
            inputs_routed: value.inputs_routed,
            default_output_name: value.default_output_name,
            last_error: value.last_error,
            routes: value.routes.into_iter().map(|route| RouteStatus {
                input_name: route.input_name,
                input_address: route.input_address,
                output_name: route.output_name,
                ready: route.ready,
                codec: route.codec,
                sample_rate: route.sample_rate,
                channels: route.channels,
                last_error: route.last_error,
            }).collect(),
        }
    }
}

fn levels(config: &Config) -> Levels {
    Levels {
        phone_gain: config.audio.phone_gain,
        desktop_gain: config.audio.desktop_gain,
        master_gain: config.audio.master_gain,
        phone_mute: config.audio.phone_mute,
        desktop_mute: config.audio.desktop_mute,
        master_mute: config.audio.master_mute,
    }
}

fn apply_controls(engine: &mut Engine, config: &Config) -> std::result::Result<(), String> {
    // เตรียม gain และ mute ก่อนเปิดเส้นทาง แต่ต้องปิดเส้นทางได้แม้ควบคุม volume ไม่สำเร็จ
    let (volume, routing) = if config.audio.routing_enabled {
        let volume = engine.set_levels(levels(config));
        let routing = engine.set_enabled(true);
        (volume, routing)
    } else {
        let routing = engine.set_enabled(false);
        let volume = engine.set_levels(levels(config));
        (volume, routing)
    };
    routing.map_err(|error| format!("Route update failed: {error}"))
        .and(volume.map_err(|error| format!("Software volume update failed: {error}")))
}

fn engine(config: &Config) -> std::result::Result<(Engine, Option<String>), String> {
    let mut engine = Engine::new()?;
    let control_error = apply_controls(&mut engine, config).err();
    Ok((engine, control_error))
}

fn diagnostic(event: &str, message: &str) {
    eprintln!("{}", serde_json::json!({ "event": event, "message": message }));
}

pub async fn run(path: PathBuf) -> Result<()> {
    ensure_user()?;
    let _lock = ipc::ControllerLock::acquire()?;
    let mut config = Config::load(&path)?;
    config.validate()?;
    let (listener, _socket_guard) = ipc::bind().await?;
    let (request_tx, mut requests) = mpsc::channel(16);
    let server = tokio::spawn(ipc::serve(listener, request_tx));
    let (bluetooth_tx, bluetooth_rx) = watch::channel(BluetoothStatus::default());
    let (a2dp_tx, a2dp_rx) = watch::channel(false);
    let monitor = tokio::spawn(bluetooth::monitor(bluetooth_tx, a2dp_rx));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).context("Cannot install termination handler")?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut engine_instance: Option<Engine> = None;
    let mut audio = AudioStatus::default();
    let mut retry_at = Instant::now();
    let mut retry_delay = 1u64;
    let mut controller_error = String::new();
    diagnostic("controller_started", &path.display().to_string());

    let outcome = loop {
        tokio::select! {
            result = &mut interrupt => break result.context("Interrupt handler failed"),
            _ = terminate.recv() => break Ok(()),
            pending = requests.recv() => {
                let Some(pending) = pending else { break Err(anyhow::anyhow!("Control socket server stopped")) };
                let response = if pending.request.config_path != path {
                    Response::error(format!("Controller uses {}; this command requested a different config", path.display()))
                } else {
                    match pending.request.command {
                        Command::Status => Response::success("Live controller status", Some(serde_json::json!({
                            "running": true,
                            "config_path": path,
                            "config": config,
                            "bluetooth": bluetooth_rx.borrow().clone(),
                            "audio": audio,
                            "policy_file_present": bluetooth::phone_policy_file_present(),
                            "policy_message": if audio.policy_ready { "Bluetooth input policy is active; output follows Ubuntu" } else if bluetooth::phone_policy_file_present() { "Input policy is installed; waiting to observe it on incoming Bluetooth audio. Log out and back in after updating the policy" } else { "Ubuntu/WirePlumber manages Bluetooth playback; bridge forwarding controls require the optional input policy" },
                            "last_error": controller_error,
                        }))),
                        Command::ConfigShow => match serde_json::to_value(&config) {
                            Ok(data) => Response::success("Active configuration", Some(data)),
                            Err(error) => Response::error(error),
                        },
                        command => {
                            let mut updated = config.clone();
                            let result = ipc::apply_command(&mut updated, &command)
                                .and_then(|()| updated.validate())
                                .and_then(|()| updated.save(&path));
                            match result {
                                Err(error) => Response::error(error),
                                Ok(()) => {
                                    config = updated;
                                    let mut engine_error = None;
                                    if let Some(engine) = engine_instance.as_mut() {
                                        engine_error = apply_controls(engine, &config).err();
                                        audio = engine.status().into();
                                    }
                                    if let Some(error) = engine_error {
                                        diagnostic("audio_control_error", &error);
                                        controller_error = error.clone();
                                        Response::error(format!("Configuration saved, but a live control could not be applied: {error}"))
                                    } else {
                                        if engine_instance.is_some() { controller_error.clear(); }
                                        Response::success("Configuration saved; audio state is available through status", None)
                                    }
                                }
                            }
                        }
                    }
                };
                let _ = pending.response.send(response);
            }
            _ = interval.tick() => {
                if engine_instance.is_none() && Instant::now() >= retry_at {
                    match engine(&config) {
                        Ok((instance, control_error)) => {
                            engine_instance = Some(instance);
                            controller_error = control_error.unwrap_or_default();
                            if !controller_error.is_empty() { diagnostic("audio_control_error", &controller_error); }
                            diagnostic("audio_engine_created", "Waiting for Bluetooth audio and the output selected by Ubuntu");
                        }
                        Err(error) => {
                            if controller_error != error { diagnostic("audio_engine_error", &error); }
                            controller_error = error;
                            retry_at = Instant::now() + Duration::from_secs(retry_delay);
                            retry_delay = (retry_delay * 2).min(30);
                        }
                    }
                }
                let tick_error = engine_instance.as_mut().and_then(|engine| engine.tick().err());
                if let Some(error) = tick_error {
                    if controller_error != error { diagnostic("audio_tick_error", &error); }
                    controller_error = error;
                    engine_instance = None;
                    audio = AudioStatus::default();
                    retry_at = Instant::now() + Duration::from_secs(retry_delay);
                    retry_delay = (retry_delay * 2).min(30);
                } else if let Some(engine) = engine_instance.as_ref() {
                    audio = engine.status().into();
                    if audio.last_error.is_empty() { controller_error.clear(); }
                    if audio.pipewire_connected && audio.inputs_detected > 0 && audio.inputs_routed == audio.inputs_detected { retry_delay = 1; }
                }
            }
        }
        a2dp_tx.send_if_modified(|enabled| {
            let next = config.audio.routing_enabled && audio.pipewire_connected;
            if *enabled == next { return false; }
            *enabled = next;
            true
        });
    };
    a2dp_tx.send_replace(false);
    monitor.abort();
    server.abort();
    if let Some(engine) = engine_instance.as_mut() {
        if let Err(error) = engine.set_enabled(false) { diagnostic("audio_shutdown_error", &error); }
    }
    drop(engine_instance);
    diagnostic("controller_stopped", "Project-owned audio resources released");
    outcome
}
