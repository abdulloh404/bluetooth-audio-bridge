# Bluetooth Audio Bridge

A CLI and optional user controller for Bluetooth audio in PipeWire. Standard installation preserves Ubuntu/WirePlumber's automatic Bluetooth playback. With the optional input policy, the controller enables or pauses incoming A2DP streams and forwards them to the output selected in Ubuntu.

BlueZ/PipeWire receive the Bluetooth audio. When the input policy is enabled, the controller owns direct forwarding links and reads Ubuntu's default output and explicit per-stream output choices. Audio keeps its existing negotiated codecs and native PipeWire processing; there is no custom PCM mixer or fixed headphone destination.

## Programs

Keep both binaries installed:

| Program | Purpose |
|---|---|
| `bluetooth-audio-bridge` | Your CLI: `status`, `select`, `volume` and the other commands below. |
| `bluetooth-audio-bridged` | Background controller launched by the user service. |
| `bluetooth-audio-bridge-phone-policy` | Optional input-rule helper, invoked explicitly. |

`bluetooth-audio-bridged` accepts `--config`, help and version options. Run `bluetooth-audio-bridge status` to query the controller.

## Requirements

- Linux with an active desktop user session and `XDG_RUNTIME_DIR`.
- Rust 1.87 or newer, Cargo, GNU Make, a C++17 compiler, CMake and pkg-config.
- PipeWire 0.3.48 or newer with development headers and SPA development headers. Ubuntu build packages: `build-essential`, `cmake`, `pkg-config`, `libpipewire-0.3-dev`, `libspa-0.2-dev`.
- An existing PipeWire/WirePlumber/BlueZ Bluetooth audio setup, including `libspa-0.2-bluetooth` on Ubuntu. The scoped routing rule supports WirePlumber 0.4 and 0.5.
- Python 3 for rule generation and util-linux (`flock` for installation/removal, `runuser` for `sudo make`).
- A paired Bluetooth A2DP source, such as an iPhone, and an available stereo audio output in Ubuntu.

## Install and start

For an upgrade, stop the existing controller yourself before replacing its files:

```sh
systemctl --user stop bluetooth-audio-bridge.service
```

From the project directory:

```sh
make install
export PATH="$HOME/.local/bin:$PATH"
```

The root Makefile delegates to `./scripts`. Installation includes a release build, both binaries, the input-rule helper and the user service. It does not install or overwrite WirePlumber rules. Existing audio settings are preserved. `sudo make install` also works from your desktop account; the scripts return to that account and use its Cargo and session paths.

Connect your Bluetooth source using Ubuntu's normal Bluetooth controls and choose an output in Ubuntu sound settings. No MAC addresses or iPhone/headphone selection are required. Installation does not start, enable or restart services. If an earlier installation left a project input rule, follow the removal steps below and log out and back in before reinstalling to restore Ubuntu's automatic routing.

Then start the controller and inspect its status:

```sh
systemctl --user daemon-reload
systemctl --user start bluetooth-audio-bridge.service
bluetooth-audio-bridge status
```

The controller stays available while waiting for Bluetooth audio, an output or the input rule. Empty legacy device fields do not prevent startup.

Stop the controller:

```sh
systemctl --user stop bluetooth-audio-bridge.service
```

Stopping removes the controller's links and releases its software-volume adjustments. Desktop applications keep their existing routes. With the optional input rule loaded, stopping also stops Bluetooth forwarding. Without that rule, Ubuntu manages Bluetooth playback independently of this service.

To start at future logins, use `systemctl --user enable bluetooth-audio-bridge.service`. Read logs with `journalctl --user -u bluetooth-audio-bridge.service -n 50 --no-pager`. For foreground use, stop the user service yourself first, then run `make run`; only one controller can own the route and configuration.

## Choose whether to forward audio

Open the CLI menu:

```sh
bluetooth-audio-bridge select
```

```text
Forward Bluetooth audio through PipeWire? Currently: on
  1) On  - use the output selected in Ubuntu
  2) Off - pause Bluetooth audio forwarding
Choose 1 or 2 [Enter keeps the current setting]:
```

For a script or a direct choice:

```sh
bluetooth-audio-bridge select on
```

Use `bluetooth-audio-bridge select off` to pause controller-owned forwarding when the optional input rule is loaded. `enable` and `disable` control the same setting. They keep the controller running and do not change systemd autostart. Offline changes are saved for the next controller start. New configurations default to forwarding on. Ubuntu's automatic Bluetooth routing remains independent when the optional rule is absent.

The old `select --iphone ... --headphones ...` syntax is replaced by this menu. Choose the audio destination in Ubuntu.

## Other CLI commands

The command names remain available:

```text
bluetooth-audio-bridge [OPTIONS] <COMMAND>

config  devices  select  status  volume  mute  enable  disable  help
```

Running the CLI without arguments shows help. It accepts `--config /absolute/path/config.toml`, `-h/--help` and `-V/--version`.

```sh
bluetooth-audio-bridge devices
bluetooth-audio-bridge config show
bluetooth-audio-bridge status
bluetooth-audio-bridge status --json
bluetooth-audio-bridge volume phone 0.4
bluetooth-audio-bridge volume desktop 0.5
bluetooth-audio-bridge volume master 0.8
bluetooth-audio-bridge mute phone on
bluetooth-audio-bridge mute phone off
```

Volume values are relative software gains from `0.0` to `1.0`. New configurations use `1.0` for every channel, preserving the original stream level. `phone` applies to incoming Bluetooth A2DP streams; `desktop` applies to existing non-Bluetooth playback streams linked to audio outputs; `master` multiplies both groups. Capture, microphone and monitor streams are excluded. The controller preserves hardware-volume settings and respects external stream-volume edits when releasing control. Missing streams or unsupported software controls are reported through status.

Status reports controller availability, the default Ubuntu output, detected/controller-routed input counts and each input's target, readiness and observed output codec/format. Multiple inputs can follow different explicit output choices. Output codecs can differ from incoming Bluetooth codecs. With standard installation, Ubuntu owns Bluetooth playback and the controller's routed count may remain zero while audio plays. `offline` means the controller is unavailable.

Make equivalents include `make select`, `make select STATE=on`, `make status`, `make devices`, `make volume CHANNEL=phone VALUE=0.4` and `make mute CHANNEL=phone STATE=on`. See `make help`.

## Input rule and output selection

The generated rule covers incoming BlueZ A2DP playback nodes, leaving headset-call capture and ordinary microphones alone. It disables automatic linking only for these Bluetooth inputs so the controller can own their lifetime. It preserves Bluetooth roles, codecs, rates and channel properties. The controller never changes Ubuntu's default output or application routes.

The rule is opt-in. It disables Ubuntu's automatic Bluetooth playback and makes forwarding depend on the running controller. Standard `make install` leaves it alone. To explicitly choose controller-owned forwarding, preview and install it separately, then log out and back in:

```sh
make phone-policy
make phone-policy-install
```

Rule locations (`XDG_CONFIG_HOME` defaults to `~/.config`):

- WirePlumber 0.4: `$XDG_CONFIG_HOME/wireplumber/bluetooth.lua.d/90-bluetooth-audio-bridge-phone.lua`.
- WirePlumber 0.5: `$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-bluetooth-audio-bridge-phone.conf`.

When opting in, update and load any rule from older fixed-device or virtual-mixer versions. Existing foreign Bluetooth links are reported instead of being removed or duplicated.

Output selection follows PipeWire's current `default.audio.sink` and explicit per-stream `target.object`/`target.node` choices. If an explicitly selected output disappears, forwarding waits for a valid selection. Suitable stereo FL/FR ports are required; unsupported layouts are reported rather than changed. The controller follows these output choices without implementing additional WirePlumber role/filter routing rules.

Bluetooth pairing, connections and reconnections remain under Ubuntu's control. PipeWire reconnection is retried by the controller without restarting any audio service.

## Configuration and removal

Configuration lives in `$XDG_CONFIG_HOME/bluetooth-audio-bridge/config.toml` with mode `0600` inside a private `0700` directory. For standalone use without installation, create it with `bluetooth-audio-bridge config init`. See [config/default.toml](config/default.toml).

Legacy device addresses, reconnect settings and mixer-only fields remain readable but are ignored and omitted when settings are saved. Existing gain, mute and forwarding choices are preserved, including gains saved as `0.5` by older versions. Set a gain to `1.0` to use that stream's original level.

Before removing the installation, end any foreground controller yourself and stop the user service:

```sh
systemctl --user stop bluetooth-audio-bridge.service
```

Then run `make uninstall` from the project directory. The uninstaller requires both current and legacy controllers to be stopped; if `bt-audio-bridge.service` is still running, it reports the exact stop command. It disables the inactive project units and removes the installed programs, user units and overrides, autostart links, generated WirePlumber rules and temporary files, and project XDG config/data/state/cache/runtime directories, including sockets and locks. Cleanup covers both `bluetooth-audio-bridge` and the former `bt-audio-bridge` names, with ownership checks before deletion. It clears the project units' failed state and refreshes the user service manager.

Ubuntu packages, Bluetooth pairings, shared audio state and logs, other projects and source/build files remain in place. For a clean reinstall, log out and back in after removal to unload the old rule from WirePlumber, then run `make install`. Standard installation preserves Ubuntu's automatic Bluetooth playback and creates fresh default project settings after an uninstall. Controller-based forwarding remains an explicit opt-in through the input policy.

## Validation scope

Build success establishes compilation only. On the actual hardware, confirm service start/stop, the selection menu, Ubuntu output changes, simultaneous Bluetooth/desktop playback, volume/mute and reconnect behavior. The controller does not implement phone calls, microphone forwarding, recording or cloud streaming.
