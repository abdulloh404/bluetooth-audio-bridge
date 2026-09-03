# Bluetooth Audio Bridge

A user service that controls the native PipeWire route from a paired iPhone to selected Bluetooth headphones. Desktop applications keep their normal routes to the same headphones, and PipeWire combines the streams. Rust provides the CLI, configuration and Bluetooth monitoring; C++ owns the direct PipeWire links and controls software levels.

The service uses the existing Bluetooth playback nodes and negotiated codecs. It creates no virtual desktop output, custom PCM mixer or additional audio processing stage. Keep the headphones selected as the output of the desktop applications you want to hear.

## Requirements

- Linux with an active desktop user session and `XDG_RUNTIME_DIR`.
- Rust 1.87 or newer, Cargo, GNU Make, a C++17 compiler, CMake and pkg-config.
- PipeWire 0.3.48 or newer with development headers and SPA development headers. Ubuntu build packages: `build-essential`, `cmake`, `pkg-config`, `libpipewire-0.3-dev`, `libspa-0.2-dev`.
- An existing PipeWire/WirePlumber/BlueZ Bluetooth audio setup, including `libspa-0.2-bluetooth` on Ubuntu. The scoped routing rule supports WirePlumber 0.4 and 0.5.
- Python 3 for rule generation and util-linux (`flock` for installation/removal, `runuser` for `sudo make`).
- An iPhone already paired as an A2DP Source and headphones already paired as an A2DP Sink through the desktop Bluetooth settings.

## First-time setup

From the project directory, build and install the programs and user service:

```sh
make install
export PATH="$HOME/.local/bin:$PATH"
bluetooth-audio-bridge devices
```

The root `Makefile` delegates to `./scripts`. `make install` includes the release build; `make build` builds only. `sudo make install` also works from your desktop account: the scripts return to that account and restore its Cargo and session paths. For custom Cargo/Rustup or XDG locations, use plain `make` or preserve those variables through `sudo`.

Installation preserves existing configuration and does not start any service. Select your paired devices using their actual addresses:

```sh
bluetooth-audio-bridge select --iphone AA:BB:CC:DD:EE:FF --headphones 11:22:33:44:55:66
bluetooth-audio-bridge config show
```

To make service stop/start control phone playback, WirePlumber must leave that phone's links to the service. Preview and install its scoped rule:

```sh
make phone-policy IPHONE=AA:BB:CC:DD:EE:FF
make phone-policy-install IPHONE=AA:BB:CC:DD:EE:FF
```

The rule disables automatic linking, reconnection of audio links and fallback for only that iPhone's A2DP playback node. It adds a direct-routing marker, and leaves the system's Bluetooth source role, codecs, audio format, desktop routes and microphone configuration in place. The service creates the same direct phone-to-headphones links once it sees the loaded rule. Existing conflicting phone links are reported instead of being duplicated or removed.

Log out and back in to load the rule, then explicitly connect the iPhone through the desktop Bluetooth settings once. After loading the rule, phone playback needs the running service. The generator never restarts audio services. If upgrading from the earlier virtual-mixer version, regenerate this rule even if an older phone rule already exists; the earlier input-mode rule is not accepted as a direct-routing rule.

Rule locations (`XDG_CONFIG_HOME` defaults to `~/.config`):

- WirePlumber 0.4: `$XDG_CONFIG_HOME/wireplumber/bluetooth.lua.d/90-bluetooth-audio-bridge-phone.lua`.
- WirePlumber 0.5: `$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-bluetooth-audio-bridge-phone.conf`.

Selecting a different iPhone requires regenerating and loading its rule. No device is automatically paired or trusted.

## Start and stop

After installation and first-time setup, load the unit and start the service:

```sh
systemctl --user daemon-reload
systemctl --user start bluetooth-audio-bridge.service
bluetooth-audio-bridge status
```

Stop the service when you want to stop its iPhone route:

```sh
systemctl --user stop bluetooth-audio-bridge.service
```

Stopping removes the service-owned phone links and releases its software-volume adjustments. Desktop playback and the existing virtual microphone continue on their own routes. The loaded phone rule keeps WirePlumber from creating a replacement phone link while the service is stopped. The service does not disconnect your headphones or stop Bluetooth, PipeWire, WirePlumber or a microphone service.

To start the service at future logins, opt in with `systemctl --user enable bluetooth-audio-bridge.service`. To inspect service logs, use `journalctl --user -u bluetooth-audio-bridge.service -n 50 --no-pager`.

For foreground use, stop the user service yourself first, then run `make run` and use Ctrl+C to stop it. Only one controller can own the configuration and route at a time.

## CLI

The command interface is retained:

```text
bluetooth-audio-bridge [OPTIONS] <COMMAND>

config  devices  select  status  volume  mute  enable  disable  help
```

Running `bluetooth-audio-bridge` without arguments shows help. Both binaries accept `--config /absolute/path/config.toml`; the CLI also supports `-h/--help` and `-V/--version`.

```sh
bluetooth-audio-bridge status
bluetooth-audio-bridge status --json
bluetooth-audio-bridge volume phone 0.4
bluetooth-audio-bridge volume desktop 0.5
bluetooth-audio-bridge volume master 0.8
bluetooth-audio-bridge mute phone on
bluetooth-audio-bridge mute phone off
bluetooth-audio-bridge disable
bluetooth-audio-bridge enable
```

`disable` removes the managed phone route while keeping the controller available; `enable` allows it again. These commands do not enable or disable systemd autostart. Configuration changes made while the controller is offline are saved for its next start.

Volume values are relative software gains from `0.0` to `1.0`. New configurations default to `1.0` for phone, desktop and master, preserving the stream's original level. Phone gain applies to the selected phone; desktop gain applies to playback streams already routed to the selected headphones; master multiplies both groups. The controller uses native software volume properties rather than changing headphone hardware volume. Stream control availability and application errors are reported through status. External volume edits are respected when releasing control. Microphone and capture streams are excluded.

Status separates controller availability, Bluetooth connection, loaded routing rule and direct-link readiness. `offline` means that the CLI could not reach this project's controller; unrelated or previously automatic Ubuntu audio can still play. The output codec, rate and channel count are read from the selected native headphone node when available. Incoming phone and outgoing headphone codecs can differ; the controller does not force either codec or claim that a configured value proves AAC is active.

The Make equivalents remain available, including `make devices`, `make status`, `make volume CHANNEL=phone VALUE=0.4`, and `make mute CHANNEL=phone STATE=on`. See `make help` for all targets.

## Configuration and recovery

Configuration is stored in `$XDG_CONFIG_HOME/bluetooth-audio-bridge/config.toml`. Installed configuration files use mode `0600` inside a private application directory with mode `0700`. For standalone binaries without installation, run `bluetooth-audio-bridge config init` first. See [config/default.toml](config/default.toml).

Older configuration files remain readable. The obsolete `virtual_sink_name`, `output_codec`, `allow_codec_fallback` and `headphone_disconnect_action` fields are omitted when configuration is saved; direct routing uses the system's existing audio nodes and codec. Existing device choices, gain and mute values are preserved, including gains saved as `0.5` by the previous version. Set a channel's gain to `1.0` to use its original stream level.

Reconnect attempts target only the selected paired A2DP devices, with bounded backoff. Automatic phone reconnection additionally requires that the current direct rule was observed in the running PipeWire session. Headphone loss removes the managed phone route; the controller does not redirect it to speakers. PipeWire loss releases old graph objects and retries the connection without restarting any system service.

## Remove

End any foreground controller yourself, or stop and disable the user service:

```sh
systemctl --user disable --now bluetooth-audio-bridge.service
```

Then remove the installation:

```sh
make uninstall
```

The uninstaller refuses to run while the controller or enabled service can still use its files. It removes the three installed programs, user unit, generated phone rules and temporary rule files, and the complete application directories under XDG config, data, state, cache and runtime, including the control socket and lock. It clears failed state for this unit and refreshes the user service manager. Cleanup failures are reported.

Ubuntu packages, Bluetooth pairings, shared audio configuration, the existing virtual microphone, source/build files and user-supplied configuration outside the application directories remain under the user's control. Log out and back in after removal to unload the scoped rule; automatic native phone routing can then resume according to your system's own policy.

## Validation scope

Build success establishes compilation only. Confirm service start/stop, simultaneous phone/desktop playback, native codec reporting, volume/mute, reconnect behavior and coexistence with the virtual microphone on the actual hardware. The controller does not implement phone calls, microphone forwarding, recording or cloud streaming.
