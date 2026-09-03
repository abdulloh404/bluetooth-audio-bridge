# Bluetooth Audio Bridge

A user service that enables or pauses incoming Bluetooth audio in PipeWire. It discovers connected A2DP playback streams automatically and forwards them to the output selected in Ubuntu. You can change outputs in Ubuntu's sound settings while the controller is running.

BlueZ/PipeWire receive the Bluetooth audio. The controller owns direct forwarding links and reads Ubuntu's default output and explicit per-stream output choices. Audio keeps its existing negotiated codecs and native PipeWire processing; there is no custom PCM mixer or fixed headphone destination.

## Programs

Keep both binaries installed:

| Program | Purpose |
|---|---|
| `bluetooth-audio-bridge` | Your CLI: `status`, `select`, `volume` and the other commands below. |
| `bluetooth-audio-bridged` | Background controller launched by the user service. |
| `bluetooth-audio-bridge-phone-policy` | Input-rule helper, also invoked by the installer. |

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

The root Makefile delegates to `./scripts`. Installation includes a release build, both binaries, the input-rule helper, the user service and the WirePlumber input rule. Existing audio settings are preserved. `sudo make install` also works from your desktop account; the scripts return to that account and use its Cargo and session paths.

**After first installation or a rule update, log out and back in to load the input rule.** Connect your Bluetooth source using Ubuntu's normal Bluetooth controls and choose an output in Ubuntu sound settings. No MAC addresses or iPhone/headphone selection are required. Installation does not start, enable or restart services.

Then start the controller and inspect its status:

```sh
systemctl --user daemon-reload
systemctl --user start bluetooth-audio-bridge.service
bluetooth-audio-bridge status
```

The controller stays available while waiting for Bluetooth audio, an output or the input rule. Empty legacy device fields do not prevent startup.

Stop forwarding Bluetooth audio:

```sh
systemctl --user stop bluetooth-audio-bridge.service
```

Stopping removes the controller's links and releases its software-volume adjustments. Desktop applications keep their existing routes. The input rule keeps automatic Bluetooth playback links from reappearing while the controller is stopped. Other audio and Bluetooth services continue running.

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

Use `bluetooth-audio-bridge select off` to pause forwarding. `enable` and `disable` control the same setting. They keep the controller running and do not change systemd autostart. Offline changes are saved for the next controller start. New configurations default to forwarding on.

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

Status reports controller availability, the default Ubuntu output, detected/routed input counts and each input's actual target, readiness and observed output codec/format. Multiple inputs can follow different explicit output choices. Output codecs can differ from incoming Bluetooth codecs. `offline` means the controller is unavailable; automatic system audio may still play if the new input rule has not been loaded.

Make equivalents include `make select`, `make select STATE=on`, `make status`, `make devices`, `make volume CHANNEL=phone VALUE=0.4` and `make mute CHANNEL=phone STATE=on`. See `make help`.

## Input rule and output selection

The generated rule covers incoming BlueZ A2DP playback nodes, leaving headset-call capture and ordinary microphones alone. It disables automatic linking only for these Bluetooth inputs so the controller can own their lifetime. It preserves Bluetooth roles, codecs, rates and channel properties. The controller never changes Ubuntu's default output or application routes.

The rule is installed automatically by `make install`. To preview or reinstall it separately:

```sh
make phone-policy
make phone-policy-install
```

Rule locations (`XDG_CONFIG_HOME` defaults to `~/.config`):

- WirePlumber 0.4: `$XDG_CONFIG_HOME/wireplumber/bluetooth.lua.d/90-bluetooth-audio-bridge-phone.lua`.
- WirePlumber 0.5: `$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-bluetooth-audio-bridge-phone.conf`.

A rule from the older fixed-device or virtual-mixer versions must be updated and loaded. Existing foreign Bluetooth links are reported instead of being removed or duplicated.

Output selection follows PipeWire's current `default.audio.sink` and explicit per-stream `target.object`/`target.node` choices. If an explicitly selected output disappears, forwarding waits for a valid selection. Suitable stereo FL/FR ports are required; unsupported layouts are reported rather than changed. The controller follows these output choices without implementing additional WirePlumber role/filter routing rules.

Bluetooth pairing, connections and reconnections remain under Ubuntu's control. PipeWire reconnection is retried by the controller without restarting any audio service.

## Configuration and removal

Configuration lives in `$XDG_CONFIG_HOME/bluetooth-audio-bridge/config.toml` with mode `0600` inside a private `0700` directory. For standalone use without installation, create it with `bluetooth-audio-bridge config init`. See [config/default.toml](config/default.toml).

Legacy device addresses, reconnect settings and mixer-only fields remain readable but are ignored and omitted when settings are saved. Existing gain, mute and forwarding choices are preserved, including gains saved as `0.5` by older versions. Set a gain to `1.0` to use that stream's original level.

Before removing the installation, end any foreground controller yourself and disable the user service:

```sh
systemctl --user disable --now bluetooth-audio-bridge.service
```

Then run `make uninstall` from the project directory. The uninstaller requires the controller to be stopped and the unit disabled. It removes the installed programs, user unit, generated rules and project-owned XDG config/data/state/cache/runtime files. It clears this unit's failed state and refreshes the user service manager.

Ubuntu packages, Bluetooth pairings, shared audio configuration, other projects and source/build files remain in place. Log out and back in after removal to unload the rule and let Ubuntu's automatic Bluetooth playback routing resume.

## Validation scope

Build success establishes compilation only. On the actual hardware, confirm service start/stop, the selection menu, Ubuntu output changes, simultaneous Bluetooth/desktop playback, volume/mute and reconnect behavior. The controller does not implement phone calls, microphone forwarding, recording or cloud streaming.
