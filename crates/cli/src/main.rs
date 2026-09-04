use anyhow::{bail, Context, Result};
use bluetooth_audio_bridge_daemon::{bluetooth, config::{self, Config}, ipc::{self, Channel, Command, Request}};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, IsTerminal, Write};
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
        #[arg(value_enum, help = "Allow or pause Bluetooth audio; omit to choose interactively")]
        state: Option<Switch>,
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

fn choose_forwarding(current: bool) -> Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Use 'bluetooth-audio-bridge select on' or 'bluetooth-audio-bridge select off' without an interactive terminal");
    }
    println!("Forward Bluetooth audio through PipeWire? Currently: {}", if current { "on" } else { "off" });
    println!("  1) On  - use the output selected in Ubuntu");
    println!("  2) Off - pause Bluetooth audio forwarding");
    print!("Choose 1 or 2 [Enter keeps the current setting]: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 { bail!("Selection cancelled; configuration unchanged"); }
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Ok(current),
        "1" | "on" | "yes" | "y" => Ok(true),
        "2" | "off" | "no" | "n" => Ok(false),
        _ => bail!("Choose 1 (on) or 2 (off); configuration unchanged"),
    }
}

fn print_status(data: &serde_json::Value) {
    let boolean = |value: &serde_json::Value| value.as_bool().unwrap_or(false);
    let text = |value: &serde_json::Value| value.as_str().filter(|value| !value.is_empty()).unwrap_or("unknown").to_owned();
    println!("Controller: running");
    println!("Config: {}", text(&data["config_path"]));
    println!("Bluetooth audio requested: {}", boolean(&data["config"]["audio"]["routing_enabled"]));
    let audio = &data["audio"];
    println!("Forwarding enabled: {} | PipeWire connected: {}", boolean(&audio["routing_enabled"]), boolean(&audio["pipewire_connected"]));
    println!("Bluetooth inputs: {} detected, {} routed", audio["inputs_detected"], audio["inputs_routed"]);
    println!("Ubuntu default output: {}", text(&audio["default_output_name"]));
    if let Some(routes) = audio["routes"].as_array() {
        for route in routes {
            println!("{} ({}) -> {} | ready: {}", text(&route["input_name"]), text(&route["input_address"]), text(&route["output_name"]), boolean(&route["ready"]));
            println!("  Native output codec: {} | sample rate: {} | channels: {}", text(&route["codec"]), route["sample_rate"], route["channels"]);
            if let Some(error) = route["last_error"].as_str().filter(|error| !error.is_empty()) { println!("  Attention: {error}"); }
        }
    }
    let settings = &data["config"]["audio"];
    for source in ["phone", "desktop", "master"] {
        println!("{source}: relative gain={} muted={}", settings[format!("{source}_gain")], boolean(&settings[format!("{source}_mute")]));
    }
    println!("Input policy: {}", text(&data["policy_message"]));
    for error in [&data["last_error"], &data["bluetooth"]["last_error"], &audio["last_error"]] {
        if let Some(error) = error.as_str().filter(|error| !error.is_empty()) { println!("Attention: {error}"); }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    config::ensure_user()?;
    let custom_config = arguments.config.is_some();
    let path = config::config_path(arguments.config)?;
    match arguments.command {
        CliCommand::Config { command: ConfigCommand::Init } => {
            let _lock = ipc::ControllerLock::acquire()?;
            if std::fs::symlink_metadata(&path).is_ok() { bail!("{} already exists; it has not been overwritten", path.display()); }
            Config::default().save(&path)?;
            println!("Created {}. Bluetooth inputs are detected automatically; choose the output in Ubuntu.", path.display());
        }
        CliCommand::Config { command: ConfigCommand::Show } => {
            let config = match remote(&path, Command::ConfigShow).await? {
                Some(response) => serde_json::from_value(response.data.context("Controller omitted configuration")?)?,
                None => Config::load(&path)?,
            };
            print!("{}", toml::to_string_pretty(&config)?);
        }
        CliCommand::Devices => {
            let settings = match std::fs::symlink_metadata(&path) {
                Ok(_) => Config::load(&path)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound && !custom_config => Config::default(),
                Err(error) => return Err(error).with_context(|| format!("Cannot read {}", path.display())),
            };
            settings.validate()?;
            let devices = bluetooth::list_devices(&settings.bluetooth).await?;
            println!("ADDRESS            PAIRED  CONNECTED  NAME");
            for device in devices {
                println!("{}  {:<6}  {:<9}  {}", device.address, device.paired, device.connected, device.name);
            }
        }
        CliCommand::Select { state } => {
            let enabled = match state {
                Some(state) => matches!(state, Switch::On),
                None => {
                    let config: Config = match remote(&path, Command::ConfigShow).await? {
                        Some(response) => serde_json::from_value(response.data.context("Controller omitted configuration")?)?,
                        None => Config::load(&path)?,
                    };
                    choose_forwarding(config.audio.routing_enabled)?
                }
            };
            change(&path, Command::Select { enabled }).await?;
            println!("Bluetooth audio forwarding: {}. Output follows Ubuntu.", if enabled { "on" } else { "off" });
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
