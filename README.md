# Bluetooth Audio Bridge

A Linux audio bridge that mixes iPhone media and desktop application audio into one selected pair of Bluetooth headphones. Rust controls configuration, BlueZ connections and commands; C++ connects and processes the PipeWire graph.

This is the initial implementation. End-to-end audio, Bluetooth stability, AAC negotiation and coexistence with the existing AirPods virtual microphone still require testing on the target hardware. The acceptance criteria are in [PROJECT-TH.md](PROJECT-TH.md) and [PROJECT-EN.md](PROJECT-EN.md).

## Requirements

- Linux with an active desktop user session and `XDG_RUNTIME_DIR`.
- Rust 1.87 or newer, Cargo, GNU Make, a C++17 compiler, CMake and pkg-config.
- PipeWire 0.3.48 or newer with development headers and SPA development headers. Ubuntu package names: `build-essential`, `cmake`, `pkg-config`, `libpipewire-0.3-dev`, `libspa-0.2-dev`.
- An existing PipeWire/WirePlumber/BlueZ audio setup, including the Bluetooth SPA plugin (`libspa-0.2-bluetooth` on Ubuntu). The phone-policy generator supports WirePlumber 0.4 and 0.5.
- Python 3 for phone-policy generation, `flock` from util-linux for installation/removal, and `runuser` from util-linux when using `sudo make`.
- Both selected devices paired explicitly through the desktop Bluetooth settings. The iPhone must advertise A2DP Source and the headphones A2DP Sink.
- AAC support in the installed Bluetooth audio stack and the selected headphones. Configuring AAC does not establish that it is available or in use.

The implementation targets the PipeWire 0.3.48 API. The development machine has PipeWire 0.3.48, WirePlumber 0.4.8 and BlueZ 5.64. No replacement Bluetooth stack or kernel module is included.

## Build and install

From this directory:

```sh
make help
make install
export PATH="$HOME/.local/bin:$PATH"
```

The root `Makefile` delegates commands to `./scripts`. `make install` first runs the release build, then installs it; use `make build` to build only. The build produces `target/release/bluetooth-audio-bridge` and `target/release/bluetooth-audio-bridged`. Installation copies these binaries, the phone-policy helper, a draft configuration and an optional user service. Installation does not start or enable services. Existing application configuration is preserved.

You can use `sudo make install` and the other Make targets from your desktop account. The scripts return to the invoking user, restore the user session paths, and find Cargo in that user's Cargo directory. `sudo ./scripts/build.sh`, `sudo ./scripts/install.sh` and `sudo ./scripts/uninstall.sh` also support this behavior. Installation remains under your home directory. For custom Cargo/Rustup or XDG locations, use plain `make` or preserve those environment variables through `sudo`.

Make controls use the installed programs in `~/.local/bin` directly. The PATH setting above allows the standalone `bluetooth-audio-bridge` commands to work outside this directory. Run the standalone daemon and CLI as your desktop user. Run `bluetooth-audio-bridge config init` if using the binaries directly without the installer.

## Select devices and prepare the phone input

List devices already known to BlueZ, then replace the two example addresses with your paired devices:

```sh
make devices
make select IPHONE=AA:BB:CC:DD:EE:FF HEADPHONES=11:22:33:44:55:66
make config
```

Device selection validates the pairing and the advertised A2DP role. The app never pairs or trusts another device automatically.

WirePlumber normally treats incoming phone media as playback that can go to the default speakers. Prepare the selected phone as a private input before playing media. The helper detects the installed PipeWire and WirePlumber versions and the Bluetooth plugin's supported input property. Preview its rule:

```sh
make phone-policy IPHONE=AA:BB:CC:DD:EE:FF
```

After reviewing the output, this explicit command saves only that phone's rule:

```sh
make phone-policy-install IPHONE=AA:BB:CC:DD:EE:FF
```

The rule is scoped to the chosen phone, disables its automatic playback routing, and lowers its source priority. Its destination is printed by the helper:

- WirePlumber 0.4: `$XDG_CONFIG_HOME/wireplumber/bluetooth.lua.d/90-bluetooth-audio-bridge-phone.lua`.
- WirePlumber 0.5: `$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-bluetooth-audio-bridge-phone.conf`.

`XDG_CONFIG_HOME` defaults to `~/.config`. The helper preserves unrelated policy files and never restarts an audio service. Log out and back in to load the rule, then explicitly connect the iPhone through the desktop Bluetooth settings. A file on disk alone is insufficient: the daemon must also observe the matching safe phone input in the running PipeWire graph before it enables automatic phone reconnection. This observation is reset when the PipeWire connection is recreated.

On old PipeWire, the input property is `bluez5.a2dp-source-role`; newer versions use `bluez5.media-source-role`. The helper detects the supported property instead of assuming they are interchangeable. See the [WirePlumber Bluetooth documentation](https://pipewire.pages.freedesktop.org/wireplumber/daemon/configuration/bluetooth.html).

## Run and control

Run the daemon in the foreground:

```sh
make run
```

Use another terminal for controls, and choose **Bluetooth Audio Bridge** as the output of the desktop applications you want to mix. You can select it as the system output yourself; the application does not set a default output or input.

```sh
make status
bluetooth-audio-bridge status --json
make volume CHANNEL=phone VALUE=0.4
make volume CHANNEL=desktop VALUE=0.5
make volume CHANNEL=master VALUE=0.8
make mute CHANNEL=phone STATE=on
make mute CHANNEL=phone STATE=off
make disable
make enable
```

`volume` accepts finite linear values from `0.0` to `1.0`. `mute` accepts `phone`, `desktop` or `master` and `on` or `off`. Levels, mute states, enable state and device choices are saved atomically in the user's configuration. While the daemon runs, commands use its private local socket. Offline changes take effect at the next launch.

The status reports Bluetooth pairing/connection separately from the native graph and stream readiness. The codec field comes from the selected live PipeWire device; it is not copied from `output_codec`. The reported PCM rate and channel count describe the bridge graph, not the compressed Bluetooth packet format. A phone that is connected but paused can remain ready with an idle stream state.

To use the installed service instead of the foreground daemon, first end the foreground session yourself, then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now bluetooth-audio-bridge.service
```

Only one controller owns the configuration and graph at a time. The daemon handles Ctrl+C and termination by removing its own streams, links and virtual sink. It never stops the existing virtual microphone or other audio services.

## Configuration and behavior

The default file is `$XDG_CONFIG_HOME/bluetooth-audio-bridge/config.toml`. Both binaries accept `--config /absolute/path/config.toml`. Custom configuration files must be owned by the desktop user, have mode `0600`, and reside in a private application directory with mode `0700`. See [config/default.toml](config/default.toml) for all settings.

| Setting or event | Behavior |
| --- | --- |
| Default source levels | Phone `0.5`, desktop `0.5`, master `1.0`; the mixer limits final samples to `[-1, 1]`. |
| `output_codec = "aac"` | Requests advertised A2DP/AAC support; playback waits when AAC cannot be confirmed. |
| `allow_codec_fallback = true` | Explicitly permits another A2DP codec. HFP is never an output fallback. |
| `auto_reconnect = true` | Retries only selected, paired devices with capped backoff. Phone retries additionally require the loaded-policy observation described above. |
| Phone disconnect or pause | Desktop audio can continue independently. |
| Headphones disconnect | The virtual desktop sink remains; forwarding is silent until the selected output returns. |
| PipeWire disconnect | The daemon waits and recreates its own graph after reconnection; it does not restart PipeWire or WirePlumber. |
| Existing phone playback links | Reports a routing conflict and refuses a duplicate route. Remove conflicting manual routes yourself. |

The C++ engine creates a desktop sink, captures its monitor and the selected phone into separate filter inputs, applies independent levels, and connects only to the selected headphone output. PipeWire handles graph mixing and resampling. No PCM buffers cross the Rust boundary. See the [PipeWire API](https://docs.pipewire.org/page_api.html) and [BlueZ Device API](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Device.rst).

If the system disables AAC or the needed A2DP roles, the status will require user action; the bridge does not rewrite global Bluetooth policy. Changing the selected phone requires generating and loading a rule for its new address.

## Remove

If using the foreground daemon, end it yourself. If using the service:

```sh
systemctl --user disable --now bluetooth-audio-bridge.service
```

Then run:

```sh
make uninstall
```

The uninstaller refuses to continue while the daemon or service is active or the service is enabled. Close other bridge commands before running it. It removes the three installed programs, the user unit, both generated phone-rule variants, abandoned phone-policy temporary files, and the complete `bluetooth-audio-bridge` directories under the user's XDG config, data, state, cache and runtime locations. This includes saved settings, temporary configuration files, the control socket and the lock. It also clears any failed state of this specific user unit and refreshes the user service manager. Cleanup errors are reported as failures instead of being silently ignored. Generated configuration and policy can also be removed when no managed binary installation remains.

Only this application's installed files and application directories are removed. Ubuntu packages, Bluetooth pairings, shared audio configuration, the existing virtual microphone and the source/build directory are preserved. User-supplied configuration files outside these application directories are not installation artifacts and remain under the user's control. No default-device change is made by the bridge, so there is no saved system default for it to overwrite on removal. Log out and back in afterward to unload the removed phone policy from the running WirePlumber session; the uninstaller does not restart audio services.

## Hardware acceptance

Build success establishes compilation only. Before treating this as a completed MVP, perform the ten listening, disconnect/reconnect, AAC and virtual-microphone checks in the project specification. The first version does not include a GUI, phone calls, FaceTime, microphone forwarding, recording or cloud streaming. Bluetooth adapter capacity and end-to-end latency must be measured on the actual hardware.
