use anyhow::{bail, Context, Result};
use bluetooth_audio_bridge_daemon::{bluetooth, config::{self, Config}, ipc::{self, Channel, Command, Request}};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(version, about = "Configure and control Bluetooth Audio Bridge")]
struct Arguments {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Devices,
    Select {
        #[arg(long)]
        iphone: String,
        #[arg(long)]
        headphones: String,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Volume { channel: Source, value: f32 },
    Mute { channel: Source, state: Switch },
    Enable,
    Disable,
}

#[derive(Subcommand)]
enum ConfigCommand { Init, Show }

#[derive(Clone, ValueEnum)]
enum Source { Phone, Desktop, Master }

impl From<Source> for Channel {
    fn from(value: Source) -> Self {
        match value { Source::Phone => Self::Phone, Source::Desktop => Self::Desktop, Source::Master => Self::Master }
    }
}

#[derive(Clone, ValueEnum)]
enum Switch { On, Off }

async fn remote(path: &Path, command: Command) -> Result<Option<ipc::Response>> {
    let response = ipc::request(&Request { config_path: path.to_owned(), command }).await?;
    if let Some(response) = &response {
        if !response.ok { bail!("{}", response.message); }
    }
    Ok(response)
}

async fn change(path: &Path, command: Command) -> Result<()> {
    if let Some(response) = remote(path, command.clone()).await? {
        println!("{}", response.message);
        return Ok(());
    }
    let _lock = ipc::ControllerLock::acquire()?;
    let mut config = Config::load(path)?;
    ipc::apply_command(&mut config, &command)?;
    config.save(path)?;
    println!("Saved {}. Controller is offline; changes apply on its next start.", path.display());
    Ok(())
}

fn print_status(data: &serde_json::Value) {
    let boolean = |value: &serde_json::Value| value.as_bool().unwrap_or(false);
    let text = |value: &serde_json::Value| value.as_str().filter(|value| !value.is_empty()).unwrap_or("unknown").to_owned();
    println!("Controller: running");
    println!("Config: {}", text(&data["config_path"]));
    println!("Routing requested: {}", boolean(&data["config"]["audio"]["routing_enabled"]));
    let audio = &data["audio"];
    println!("Direct route management enabled: {}", boolean(&audio["routing_enabled"]));
    println!("PipeWire connected: {} | direct phone-to-headphones route ready: {}", boolean(&audio["pipewire_connected"]), boolean(&audio["route_ready"]));
    for (key, label, ready) in [("phone", "Phone", "phone_ready"), ("headphones", "Headphones", "headphones_ready")] {
        let device = &data["bluetooth"][key];
        println!("{label}: {} | paired: {} | connected: {} | audio ready: {}", text(&device["address"]), boolean(&device["paired"]), boolean(&device["connected"]), boolean(&audio[ready]));
        println!("  {}", text(&data["bluetooth"][format!("{key}_reconnect")]));
    }
    println!("Native output codec: {} | sample rate: {} | channels: {}", text(&audio["codec"]), audio["sample_rate"], audio["channels"]);
    println!("Phone stream: {} | output stream: {}", text(&audio["phone_stream_state"]), text(&audio["output_stream_state"]));
    let settings = &data["config"]["audio"];
    for source in ["phone", "desktop", "master"] {
        println!("{source}: relative gain={} muted={}", settings[format!("{source}_gain")], boolean(&settings[format!("{source}_mute")]));
    }
    println!("Phone policy: {}", text(&data["phone_policy_message"]));
    for error in [&data["last_error"], &data["bluetooth"]["last_error"], &audio["last_error"]] {
        if let Some(error) = error.as_str().filter(|error| !error.is_empty()) { println!("Attention: {error}"); }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    config::ensure_user()?;
    let path = config::config_path(arguments.config)?;
    match arguments.command {
        CliCommand::Config { command: ConfigCommand::Init } => {
            let _lock = ipc::ControllerLock::acquire()?;
            if std::fs::symlink_metadata(&path).is_ok() { bail!("{} already exists; it has not been overwritten", path.display()); }
            Config::default().save(&path)?;
            println!("Created {}. Select both paired devices before starting the controller.", path.display());
        }
        CliCommand::Config { command: ConfigCommand::Show } => {
            let config = match remote(&path, Command::ConfigShow).await? {
                Some(response) => serde_json::from_value(response.data.context("Controller omitted configuration")?)?,
                None => Config::load(&path)?,
            };
            print!("{}", toml::to_string_pretty(&config)?);
        }
        CliCommand::Devices => {
            let devices = bluetooth::list_devices().await?;
            println!("ADDRESS            PAIRED  CONNECTED  NAME");
            for device in devices {
                println!("{}  {:<6}  {:<9}  {}", device.address, device.paired, device.connected, device.name);
            }
        }
        CliCommand::Select { iphone, headphones } => {
            let iphone_address = config::normalize_address(&iphone)?;
            let headphones_address = config::normalize_address(&headphones)?;
            if iphone_address == headphones_address { bail!("iPhone and headphones must be distinct devices"); }
            bluetooth::validate_selection(&iphone_address, &headphones_address).await?;
            change(&path, Command::Select { iphone_address, headphones_address }).await?;
        }
        CliCommand::Status { json } => {
            match remote(&path, Command::Status).await? {
                Some(response) => {
                    let data = response.data.context("Controller omitted status")?;
                    if json { println!("{}", serde_json::to_string_pretty(&data)?); } else { print_status(&data); }
                }
                None => {
                    if json {
                        println!("{}", serde_json::json!({ "running": false, "config_path": path, "message": "Controller is offline; Ubuntu audio may still be playing independently", "start_command": "systemctl --user start bluetooth-audio-bridge.service" }));
                    } else {
                        println!("Controller is offline; Ubuntu audio may still be playing independently.");
                        println!("Start the controller: systemctl --user start bluetooth-audio-bridge.service");
                    }
                }
            }
        }
        CliCommand::Volume { channel, value } => {
            config::validate_gain(value)?;
            change(&path, Command::Volume { channel: channel.into(), value }).await?;
        }
        CliCommand::Mute { channel, state } => change(&path, Command::Mute { channel: channel.into(), muted: matches!(state, Switch::On) }).await?,
        CliCommand::Enable => change(&path, Command::Enable { enabled: true }).await?,
        CliCommand::Disable => change(&path, Command::Enable { enabled: false }).await?,
    }
    Ok(())
}
