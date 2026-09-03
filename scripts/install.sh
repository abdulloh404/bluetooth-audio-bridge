#!/bin/sh
set -eu
umask 077

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app_uid=$(id -u)
if [ "$app_uid" -eq 0 ]; then
    exec "$project_dir/scripts/make.sh" install "$@"
fi

bin_dir="$HOME/.local/bin"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/bluetooth-audio-bridge"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/bluetooth-audio-bridge"
runtime_dir="${XDG_RUNTIME_DIR:?Run this installer from your desktop user session}/bluetooth-audio-bridge"

for executable in bluetooth-audio-bridge bluetooth-audio-bridged; do
    if [ ! -x "$project_dir/target/release/$executable" ]; then
        printf '%s\n' "Missing release binary: $executable. Run make build first." >&2
        exit 1
    fi
done

if [ -L "$runtime_dir" ] || [ -L "$runtime_dir/controller.lock" ]; then
    printf '%s\n' 'Refusing a symbolic link in the runtime lock path.' >&2
    exit 1
fi
case "$runtime_dir" in
    /*) ;;
    *) printf '%s\n' 'XDG_RUNTIME_DIR must be absolute.' >&2; exit 1 ;;
esac
if [ ! -d "$runtime_dir" ]; then
    mkdir -m 700 -- "$runtime_dir"
fi
if [ "$(stat -c '%u:%a' "$runtime_dir")" != "$app_uid:700" ] || { [ -e "$runtime_dir/controller.lock" ] && [ ! -f "$runtime_dir/controller.lock" ]; }; then
    printf '%s\n' 'Expected a private user-owned runtime directory and a regular controller lock.' >&2
    exit 1
fi
exec 9<>"$runtime_dir/controller.lock"
if ! flock -n 9; then
    printf '%s\n' 'Bluetooth Audio Bridge is running or another command is updating it. Close it yourself before installing.' >&2
    exit 1
fi
if [ "$(stat -Lc '%d:%i' /proc/self/fd/9)" != "$(stat -c '%d:%i' "$runtime_dir/controller.lock")" ]; then
    printf '%s\n' 'The runtime lock changed during cleanup. Retry installing.' >&2
    exit 1
fi

for item in "$config_dir" "$data_dir" "$data_dir/install-marker" "$config_dir/config.toml" "$unit_dir/bluetooth-audio-bridge.service" "$bin_dir/bluetooth-audio-bridge" "$bin_dir/bluetooth-audio-bridged" "$bin_dir/bluetooth-audio-bridge-phone-policy"; do
    if [ -L "$item" ]; then
        printf '%s\n' "Refusing to replace a symbolic link: $item" >&2
        exit 1
    fi
done

if [ -f "$data_dir/install-marker" ] && [ "$(cat "$data_dir/install-marker")" = 'BLUETOOTH_AUDIO_BRIDGE_INSTALL=1' ]; then
    :
else
    for item in "$bin_dir/bluetooth-audio-bridge" "$bin_dir/bluetooth-audio-bridged" "$bin_dir/bluetooth-audio-bridge-phone-policy" "$unit_dir/bluetooth-audio-bridge.service"; do
        if [ -e "$item" ]; then
            printf '%s\n' "Refusing to overwrite an existing unmanaged file: $item" >&2
            exit 1
        fi
    done
fi

mkdir -p "$bin_dir" "$unit_dir"
install -d -m 700 "$config_dir" "$data_dir"
# บันทึก ownership ก่อนคัดลอก เพื่อให้ uninstall จัดการ installation ที่ถูกขัดจังหวะได้
printf '%s\n' 'BLUETOOTH_AUDIO_BRIDGE_INSTALL=1' > "$data_dir/install-marker"
install -m 755 "$project_dir/target/release/bluetooth-audio-bridge" "$bin_dir/bluetooth-audio-bridge"
install -m 755 "$project_dir/target/release/bluetooth-audio-bridged" "$bin_dir/bluetooth-audio-bridged"
install -m 755 "$project_dir/scripts/phone-policy.sh" "$bin_dir/bluetooth-audio-bridge-phone-policy"
install -m 644 "$project_dir/systemd/bluetooth-audio-bridge.service" "$unit_dir/bluetooth-audio-bridge.service"
if [ ! -e "$config_dir/config.toml" ]; then
    install -m 600 "$project_dir/config/default.toml" "$config_dir/config.toml"
fi
"$project_dir/scripts/phone-policy.sh" --install
printf '%s\n' "Installed in $bin_dir" "Configuration: $config_dir/config.toml"
printf '%s\n' 'Log out and back in to load the Bluetooth input rule. Choose the output in Ubuntu; no device selection is required.'
printf '%s\n' 'Use bluetooth-audio-bridge select to choose whether to forward Bluetooth audio.'
printf '%s\n' 'Service commands after setup:' '  systemctl --user daemon-reload' '  systemctl --user start bluetooth-audio-bridge.service' '  systemctl --user stop bluetooth-audio-bridge.service' '  systemctl --user enable --now bluetooth-audio-bridge.service'
printf '%s\n' 'Use bluetooth-audio-bridge status for route readiness. No service was enabled or started.'
