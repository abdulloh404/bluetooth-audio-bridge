use crate::bluetooth::{self, BluetoothStatus, MonitorConfig};
use crate::config::{ensure_user, Config};
use crate::ipc::{self, Command, Response};
use anyhow::{Context, Result};
use bluetooth_audio_bridge_audio::{Engine, EngineConfig, Levels};
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[derive(Default, Serialize)]
struct AudioStatus {
    pipewire_connected: bool,
    virtual_sink_ready: bool,
    phone_ready: bool,
    headphones_ready: bool,
    routing_enabled: bool,
    codec: String,
    sample_rate: u32,
    channels: u32,
    phone_stream_state: String,
    output_stream_state: String,
    last_error: String,
}

impl From<bluetooth_audio_bridge_audio::Status> for AudioStatus {
    fn from(value: bluetooth_audio_bridge_audio::Status) -> Self {
        Self {
            pipewire_connected: value.pipewire_connected,
            virtual_sink_ready: value.virtual_sink_ready,
            phone_ready: value.phone_ready,
            headphones_ready: value.headphones_ready,
            routing_enabled: value.routing_enabled,
            codec: value.codec,
            sample_rate: value.sample_rate,
            channels: value.channels,
            phone_stream_state: value.phone_stream_state,
            output_stream_state: value.output_stream_state,
            last_error: value.last_error,
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

fn engine(config: &Config) -> std::result::Result<Engine, String> {
    let mut engine = Engine::new(&EngineConfig {
        virtual_sink_name: config.audio.virtual_sink_name.clone(),
        iphone_address: config.devices.iphone_address.clone(),
        headphones_address: config.devices.headphones_address.clone(),
        allow_codec_fallback: config.audio.allow_codec_fallback,
    })?;
    engine.set_levels(levels(config))?;
    engine.set_enabled(config.audio.routing_enabled)?;
    Ok(engine)
}

fn diagnostic(event: &str, message: &str) {
    eprintln!("{}", serde_json::json!({ "event": event, "message": message }));
}

pub async fn run(path: PathBuf) -> Result<()> {
    ensure_user()?;
    let _lock = ipc::ControllerLock::acquire()?;
    let mut config = Config::load(&path)?;
    config.validate(true)?;
    let (listener, _socket_guard) = ipc::bind().await?;
    let (request_tx, mut requests) = mpsc::channel(16);
    let server = tokio::spawn(ipc::serve(listener, request_tx));
    let (monitor_tx, monitor_rx) = watch::channel(MonitorConfig { config: config.clone(), phone_policy_observed: false });
    let (bluetooth_tx, bluetooth_rx) = watch::channel(BluetoothStatus::default());
    let monitor = tokio::spawn(bluetooth::monitor(monitor_rx, bluetooth_tx));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).context("Cannot install termination handler")?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut engine_instance: Option<Engine> = None;
    let mut audio = AudioStatus::default();
    let mut retry_at = Instant::now();
    let mut retry_delay = 1u64;
    let mut phone_policy_observed = false;
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
                            "phone_policy_file_present": bluetooth::phone_policy_file_present(&config.devices.iphone_address),
                            "phone_policy_observed": phone_policy_observed,
                            "phone_policy_message": if phone_policy_observed { "Safe phone input observed in current PipeWire connection" } else { "Install the scoped phone policy, load it in WirePlumber, then connect the phone explicitly once" },
                            "last_error": controller_error,
                        }))),
                        Command::ConfigShow => match serde_json::to_value(&config) {
                            Ok(data) => Response::success("Active configuration", Some(data)),
                            Err(error) => Response::error(error),
                        },
                        command => {
                            let mut updated = config.clone();
                            let result = ipc::apply_command(&mut updated, &command)
                                .and_then(|()| updated.validate(true))
                                .and_then(|()| updated.save(&path));
                            match result {
                                Err(error) => Response::error(error),
                                Ok(()) => {
                                    let recreate = matches!(command, Command::Select { .. });
                                    config = updated;
                                    let mut engine_error = None;
                                    if recreate {
                                        engine_instance = None;
                                        audio = AudioStatus::default();
                                        phone_policy_observed = false;
                                        retry_at = Instant::now();
                                        retry_delay = 1;
                                    } else if let Some(engine) = engine_instance.as_mut() {
                                        engine_error = engine.set_levels(levels(&config))
                                            .and_then(|()| engine.set_enabled(config.audio.routing_enabled)).err();
                                        if engine_error.is_none() { audio = engine.status().into(); }
                                    }
                                    if let Some(error) = engine_error {
                                        diagnostic("audio_control_error", &error);
                                        controller_error = error;
                                        engine_instance = None;
                                        audio = AudioStatus::default();
                                        phone_policy_observed = false;
                                        retry_at = Instant::now() + Duration::from_secs(1);
                                    }
                                    monitor_tx.send_replace(MonitorConfig { config: config.clone(), phone_policy_observed });
                                    Response::success("Configuration saved; audio state is available through status", None)
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
                        Ok(instance) => {
                            engine_instance = Some(instance);
                            controller_error.clear();
                            diagnostic("audio_engine_created", "Waiting for PipeWire graph readiness");
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
                    if audio.pipewire_connected && audio.virtual_sink_ready { retry_delay = 1; }
                }
                let observed = audio.pipewire_connected && (phone_policy_observed || audio.phone_ready);
                if observed != phone_policy_observed {
                    phone_policy_observed = observed;
                    monitor_tx.send_replace(MonitorConfig { config: config.clone(), phone_policy_observed });
                }
            }
        }
    };
    monitor.abort();
    server.abort();
    if let Some(engine) = engine_instance.as_mut() {
        if let Err(error) = engine.set_enabled(false) { diagnostic("audio_shutdown_error", &error); }
    }
    drop(engine_instance);
    diagnostic("controller_stopped", "Project-owned audio resources released");
    outcome
}
