#!/bin/sh
set -eu

if [ "$(id -u)" -eq 0 ]; then
    printf '%s\n' 'Run this uninstaller as the desktop user, not root.' >&2
    exit 1
fi

bin_dir="$HOME/.local/bin"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
config_dir="$config_home/bt-audio-bridge"
unit_dir="$config_home/systemd/user"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/bt-audio-bridge"
runtime_dir="${XDG_RUNTIME_DIR:?Run this uninstaller from your desktop user session}/bt-audio-bridge"

if [ -L "$data_dir" ] || [ -L "$data_dir/install-marker" ] || [ ! -f "$data_dir/install-marker" ] || [ "$(cat "$data_dir/install-marker")" != 'BT_AUDIO_BRIDGE_INSTALL=1' ]; then
    printf '%s\n' 'No managed BT Audio Bridge installation was found; nothing was removed.' >&2
    exit 1
fi

if [ -L "$runtime_dir" ] || [ -L "$runtime_dir/controller.lock" ]; then
    printf '%s\n' 'Refusing a symbolic link in the runtime lock path.' >&2
    exit 1
fi
if [ -f "$runtime_dir/controller.lock" ]; then
    exec 9<"$runtime_dir/controller.lock"
    if ! flock -n 9; then
        printf '%s\n' 'BT Audio Bridge is running. Stop it yourself before uninstalling.' >&2
        exit 1
    fi
fi
if systemctl --user is-active --quiet bt-audio-bridge.service; then
    printf '%s\n' 'The user service is active. Stop and disable it yourself before uninstalling.' >&2
    exit 1
fi
if systemctl --user is-enabled --quiet bt-audio-bridge.service; then
    printf '%s\n' 'The user service is enabled. Disable it yourself before uninstalling.' >&2
    exit 1
fi

for item in "$config_dir" "$config_dir/config.toml" "$unit_dir/bt-audio-bridge.service" "$bin_dir/bt-audio-bridge" "$bin_dir/bt-audio-bridged" "$bin_dir/bt-audio-bridge-phone-policy"; do
    if [ -L "$item" ]; then
        printf '%s\n' "Refusing to remove a symbolic link: $item" >&2
        exit 1
    fi
done

for policy in "$config_home/wireplumber/bluetooth.lua.d/90-bt-audio-bridge-phone.lua" "$config_home/wireplumber/wireplumber.conf.d/90-bt-audio-bridge-phone.conf"; do
    if [ -f "$policy" ] && [ ! -L "$policy" ]; then
        first_line=$(head -n 1 "$policy")
        case "$first_line" in
            '-- BT_AUDIO_BRIDGE_POLICY=1'|'# BT_AUDIO_BRIDGE_POLICY=1') rm -- "$policy" ;;
            *) printf '%s\n' "Preserved an unmanaged policy: $policy" ;;
        esac
    fi
done

rm -f -- "$bin_dir/bt-audio-bridge" "$bin_dir/bt-audio-bridged" "$bin_dir/bt-audio-bridge-phone-policy" "$unit_dir/bt-audio-bridge.service" "$config_dir/config.toml" "$data_dir/install-marker"
if [ -S "$runtime_dir/control.sock" ] && [ ! -L "$runtime_dir/control.sock" ]; then
    rm -- "$runtime_dir/control.sock"
fi
rmdir "$config_dir" "$data_dir" 2>/dev/null || true

printf '%s\n' 'Removed BT Audio Bridge binaries, service, configuration and generated phone policy.'
printf '%s\n' 'Bluetooth pairings and unrelated audio settings were preserved.'
printf '%s\n' 'No service was stopped or restarted. Refresh user units with: systemctl --user daemon-reload'
printf '%s\n' 'The removed phone policy remains loaded until your next desktop session.'
