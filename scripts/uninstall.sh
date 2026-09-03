#!/bin/sh
set -eu
umask 077

app_uid=$(id -u)
if [ "$app_uid" -eq 0 ]; then
    project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
    exec "$project_dir/scripts/make.sh" uninstall "$@"
fi

bin_dir="$HOME/.local/bin"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
config_dir="$config_home/bt-audio-bridge"
unit_dir="$config_home/systemd/user"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/bt-audio-bridge"
state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/bt-audio-bridge"
cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/bt-audio-bridge"
runtime_dir="${XDG_RUNTIME_DIR:?Run this uninstaller from your desktop user session}/bt-audio-bridge"

for directory in "$bin_dir" "$unit_dir" "$config_dir" "$data_dir" "$state_dir" "$cache_dir" "$runtime_dir"; do
    case "$directory" in
        /*) ;;
        *) printf '%s\n' "Refusing a relative installation path: $directory" >&2; exit 1 ;;
    esac
done

for directory in "$config_dir" "$data_dir" "$state_dir" "$cache_dir" "$runtime_dir"; do
    if [ -L "$directory" ]; then
        printf '%s\n' "Refusing a symbolic link at an application directory: $directory" >&2
        exit 1
    fi
    if [ -e "$directory" ] && { [ ! -d "$directory" ] || [ "$(stat -c '%u' "$directory")" != "$app_uid" ]; }; then
        printf '%s\n' "Expected an application directory owned by this user: $directory" >&2
        exit 1
    fi
done

managed_install=false
if [ -e "$data_dir/install-marker" ] || [ -L "$data_dir/install-marker" ]; then
    if [ -L "$data_dir/install-marker" ] || [ ! -f "$data_dir/install-marker" ] || [ "$(cat "$data_dir/install-marker")" != 'BT_AUDIO_BRIDGE_INSTALL=1' ]; then
        printf '%s\n' 'The installation marker is invalid; nothing was removed.' >&2
        exit 1
    fi
    managed_install=true
fi

for item in "$unit_dir/bt-audio-bridge.service" "$bin_dir/bt-audio-bridge" "$bin_dir/bt-audio-bridged" "$bin_dir/bt-audio-bridge-phone-policy"; do
    if [ -L "$item" ] || { [ -e "$item" ] && { [ "$managed_install" != true ] || [ ! -f "$item" ] || [ "$(stat -c '%u' "$item")" != "$app_uid" ]; }; }; then
        printf '%s\n' "Refusing to remove an unmanaged file: $item" >&2
        exit 1
    fi
done

for policy in "$config_home/wireplumber/bluetooth.lua.d/90-bt-audio-bridge-phone.lua" "$config_home/wireplumber/wireplumber.conf.d/90-bt-audio-bridge-phone.conf"; do
    if [ -L "$policy" ] || { [ -e "$policy" ] && { [ ! -f "$policy" ] || [ "$(stat -c '%u' "$policy")" != "$app_uid" ]; }; }; then
        printf '%s\n' "Refusing to remove an unmanaged policy: $policy" >&2
        exit 1
    fi
    if [ -f "$policy" ]; then
        case "$(head -n 1 "$policy")" in
            '-- BT_AUDIO_BRIDGE_POLICY=1'|'# BT_AUDIO_BRIDGE_POLICY=1') ;;
            *) printf '%s\n' "Policy is not owned by BT Audio Bridge: $policy" >&2; exit 1 ;;
        esac
    fi
done

if ! systemctl --user show-environment >/dev/null; then
    printf '%s\n' 'Cannot contact the user service manager to check and clear the installed unit; nothing was removed.' >&2
    exit 1
fi
if systemctl --user is-active --quiet bt-audio-bridge.service; then
    printf '%s\n' 'The user service is active. Stop and disable it yourself before uninstalling.' >&2
    exit 1
fi
if systemctl --user is-enabled --quiet bt-audio-bridge.service; then
    printf '%s\n' 'The user service is enabled. Disable it yourself before uninstalling.' >&2
    exit 1
fi

if [ ! -d "$runtime_dir" ]; then
    mkdir -m 700 -- "$runtime_dir"
fi
if [ "$(stat -c '%a' "$runtime_dir")" != 700 ] || [ -L "$runtime_dir/controller.lock" ] || { [ -e "$runtime_dir/controller.lock" ] && [ ! -f "$runtime_dir/controller.lock" ]; }; then
    printf '%s\n' 'Expected a private runtime directory and a regular controller lock.' >&2
    exit 1
fi
exec 9<>"$runtime_dir/controller.lock"
if ! flock -n 9; then
    printf '%s\n' 'BT Audio Bridge is running or another command is updating it. Close it yourself before uninstalling.' >&2
    exit 1
fi
lock_identity=$(stat -Lc '%d:%i' /proc/self/fd/9)
if [ "$lock_identity" != "$(stat -c '%d:%i' "$runtime_dir/controller.lock")" ]; then
    printf '%s\n' 'The runtime lock changed during cleanup. Retry uninstalling.' >&2
    exit 1
fi

if systemctl --user is-failed --quiet bt-audio-bridge.service; then
    systemctl --user reset-failed bt-audio-bridge.service
fi

for policy in "$config_home/wireplumber/bluetooth.lua.d/90-bt-audio-bridge-phone.lua" "$config_home/wireplumber/wireplumber.conf.d/90-bt-audio-bridge-phone.conf"; do
    rm -f -- "$policy"
done

for temporary in "$config_home/wireplumber/bluetooth.lua.d"/.bt-audio-bridge-* "$config_home/wireplumber/wireplumber.conf.d"/.bt-audio-bridge-*; do
    if [ ! -e "$temporary" ] && [ ! -L "$temporary" ]; then
        continue
    fi
    if [ -L "$temporary" ] || [ ! -f "$temporary" ] || [ "$(stat -c '%u' "$temporary")" != "$app_uid" ]; then
        printf '%s\n' "Cannot safely remove the phone-policy temporary file: $temporary" >&2
        exit 1
    fi
    rm -- "$temporary"
done

rm -f -- "$bin_dir/bt-audio-bridge" "$bin_dir/bt-audio-bridged" "$bin_dir/bt-audio-bridge-phone-policy" "$unit_dir/bt-audio-bridge.service"
rm -rf --one-file-system -- "$config_dir" "$data_dir" "$state_dir" "$cache_dir"
systemctl --user daemon-reload

for item in "$runtime_dir"/* "$runtime_dir"/.[!.]* "$runtime_dir"/..?*; do
    if [ "$item" = "$runtime_dir/controller.lock" ] || { [ ! -e "$item" ] && [ ! -L "$item" ]; }; then
        continue
    fi
    rm -rf --one-file-system -- "$item"
done
if [ "$lock_identity" != "$(stat -c '%d:%i' "$runtime_dir/controller.lock")" ]; then
    printf '%s\n' 'The runtime lock was replaced by another command. Cleanup is incomplete; close the command and retry.' >&2
    exit 1
fi
rm -- "$runtime_dir/controller.lock"
if ! rmdir -- "$runtime_dir"; then
    printf '%s\n' 'The runtime directory could not be removed. Cleanup is incomplete; close other bridge commands and retry.' >&2
    exit 1
fi

printf '%s\n' 'Removed the managed binaries, user unit, generated phone policy, application configuration, data, state, cache and runtime directory (including socket and lock).'
printf '%s\n' 'The user service manager was refreshed. No service was stopped or restarted.'
printf '%s\n' 'Ubuntu packages, Bluetooth pairings, shared audio configuration and the source/build directory were preserved.'
printf '%s\n' 'Log out and back in to unload the phone policy already held by WirePlumber.'
