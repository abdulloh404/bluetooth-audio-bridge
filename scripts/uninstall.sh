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
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
state_home="${XDG_STATE_HOME:-$HOME/.local/state}"
cache_home="${XDG_CACHE_HOME:-$HOME/.cache}"
runtime_home="${XDG_RUNTIME_DIR:?Run this uninstaller from your desktop user session}"
unit_dir="$config_home/systemd/user"
runtime_unit_dir="$runtime_home/systemd/user"
policy_lua_dir="$config_home/wireplumber/bluetooth.lua.d"
policy_conf_dir="$config_home/wireplumber/wireplumber.conf.d"
app_names='bluetooth-audio-bridge bt-audio-bridge'

select_app() {
    app_name=$1
    config_dir="$config_home/$app_name"
    data_dir="$data_home/$app_name"
    state_dir="$state_home/$app_name"
    cache_dir="$cache_home/$app_name"
    runtime_dir="$runtime_home/$app_name"
    case "$app_name" in
        bluetooth-audio-bridge) marker_prefix=BLUETOOTH_AUDIO_BRIDGE ;;
        bt-audio-bridge) marker_prefix=BT_AUDIO_BRIDGE ;;
    esac
}

# ตรวจไฟล์ทั้งชื่อปัจจุบันและชื่อเดิมก่อนเริ่มลบ เพื่อไม่ถอนค้างครึ่งทางจากไฟล์ที่ไม่ใช่ของโปรเจกต์
for app_name in $app_names; do
    select_app "$app_name"
    for directory in "$bin_dir" "$unit_dir" "$runtime_unit_dir" "$config_dir" "$data_dir" "$state_dir" "$cache_dir" "$runtime_dir"; do
        case "$directory" in
            /*) ;;
            *) printf '%s\n' "Refusing a relative installation path: $directory" >&2; exit 1 ;;
        esac
    done
    for directory in "$config_dir" "$data_dir" "$state_dir" "$cache_dir" "$runtime_dir" "$unit_dir/$app_name.service.d" "$runtime_unit_dir/$app_name.service.d"; do
        if [ -L "$directory" ] || { [ -e "$directory" ] && { [ ! -d "$directory" ] || [ "$(stat -c '%u' "$directory")" != "$app_uid" ]; }; }; then
            printf '%s\n' "Expected a user-owned application directory, not a symbolic link: $directory" >&2
            exit 1
        fi
    done

    managed_install=false
    if [ -e "$data_dir/install-marker" ] || [ -L "$data_dir/install-marker" ]; then
        if [ -L "$data_dir/install-marker" ] || [ ! -f "$data_dir/install-marker" ] || [ "$(stat -c '%u' "$data_dir/install-marker")" != "$app_uid" ] || [ "$(cat "$data_dir/install-marker")" != "${marker_prefix}_INSTALL=1" ]; then
            printf '%s\n' "Invalid installation marker: $data_dir/install-marker. Nothing was removed." >&2
            exit 1
        fi
        managed_install=true
    fi

    for item in "$unit_dir/$app_name.service" "$bin_dir/$app_name" "$bin_dir/${app_name}d" "$bin_dir/$app_name-phone-policy"; do
        if [ -L "$item" ] || { [ -e "$item" ] && { [ "$managed_install" != true ] || [ ! -f "$item" ] || [ "$(stat -c '%u' "$item")" != "$app_uid" ]; }; }; then
            printf '%s\n' "Refusing to remove an unmanaged file: $item" >&2
            exit 1
        fi
    done

    for policy in "$policy_lua_dir/90-$app_name-phone.lua" "$policy_conf_dir/90-$app_name-phone.conf"; do
        if [ -L "$policy" ] || { [ -e "$policy" ] && { [ ! -f "$policy" ] || [ "$(stat -c '%u' "$policy")" != "$app_uid" ]; }; }; then
            printf '%s\n' "Refusing to remove an unmanaged policy: $policy" >&2
            exit 1
        fi
        if [ -f "$policy" ]; then
            first_line=$(head -n 1 "$policy")
            if [ "$first_line" != "-- ${marker_prefix}_POLICY=1" ] && [ "$first_line" != "# ${marker_prefix}_POLICY=1" ]; then
                printf '%s\n' "Policy is not owned by $app_name: $policy" >&2
                exit 1
            fi
        fi
    done
    for temporary in "$policy_lua_dir/.$app_name-"* "$policy_conf_dir/.$app_name-"*; do
        if [ ! -e "$temporary" ] && [ ! -L "$temporary" ]; then continue; fi
        if [ -L "$temporary" ] || [ ! -f "$temporary" ] || [ "$(stat -c '%u' "$temporary")" != "$app_uid" ]; then
            printf '%s\n' "Cannot safely remove the phone-policy temporary file: $temporary" >&2
            exit 1
        fi
    done

    for link_dir in "$unit_dir"/*.wants "$unit_dir"/*.requires "$runtime_unit_dir"/*.wants "$runtime_unit_dir"/*.requires; do
        item="$link_dir/$app_name.service"
        if [ ! -e "$item" ] && [ ! -L "$item" ]; then continue; fi
        if [ ! -L "$item" ] || [ "$(stat -c '%u' "$item")" != "$app_uid" ] || [ "$(readlink -m -- "$item")" != "$(readlink -m -- "$unit_dir/$app_name.service")" ]; then
            printf '%s\n' "Refusing to remove an unmanaged service link: $item" >&2
            exit 1
        fi
    done
done

if ! systemctl --user show-environment >/dev/null; then
    printf '%s\n' 'Cannot contact the user service manager; nothing was removed.' >&2
    exit 1
fi
for app_name in $app_names; do
    service_state=$(systemctl --user show --property=ActiveState --value "$app_name.service")
    case "$service_state" in
        inactive|failed) ;;
        *) printf '%s\n' "Stop $app_name.service yourself before uninstalling: systemctl --user stop $app_name.service" >&2; exit 1 ;;
    esac
done

# ถือ lock ทั้งสองชื่อไว้จนลบเสร็จ เพื่อไม่ให้ controller หรือ installer สร้างไฟล์กลับระหว่างถอน
for app_name in $app_names; do
    select_app "$app_name"
    if [ ! -d "$runtime_dir" ]; then mkdir -m 700 -- "$runtime_dir"; fi
    if [ "$(stat -c '%u:%a' "$runtime_dir")" != "$app_uid:700" ] || [ -L "$runtime_dir/controller.lock" ] || { [ -e "$runtime_dir/controller.lock" ] && { [ ! -f "$runtime_dir/controller.lock" ] || [ "$(stat -c '%u' "$runtime_dir/controller.lock")" != "$app_uid" ]; }; }; then
        printf '%s\n' "Expected a private runtime directory and a user-owned regular lock: $runtime_dir" >&2
        exit 1
    fi
    case "$app_name" in
        bluetooth-audio-bridge)
            exec 9<>"$runtime_dir/controller.lock"
            lock_fd=9
            current_lock_identity=$(stat -Lc '%d:%i' /proc/self/fd/9)
            lock_identity=$current_lock_identity
            ;;
        bt-audio-bridge)
            exec 8<>"$runtime_dir/controller.lock"
            lock_fd=8
            legacy_lock_identity=$(stat -Lc '%d:%i' /proc/self/fd/8)
            lock_identity=$legacy_lock_identity
            ;;
    esac
    if ! flock -n "$lock_fd"; then
        printf '%s\n' "$app_name is running or another command is updating it. Close it yourself before uninstalling." >&2
        exit 1
    fi
    if [ "$lock_identity" != "$(stat -c '%d:%i' "$runtime_dir/controller.lock")" ]; then
        printf '%s\n' "The $app_name runtime lock changed during cleanup. Retry uninstalling." >&2
        exit 1
    fi
done

for app_name in $app_names; do
    select_app "$app_name"
    if [ -f "$unit_dir/$app_name.service" ]; then
        systemctl --user --no-reload disable "$app_name.service"
    fi
    if systemctl --user is-failed --quiet "$app_name.service"; then
        systemctl --user reset-failed "$app_name.service"
    fi
    for link_dir in "$unit_dir"/*.wants "$unit_dir"/*.requires "$runtime_unit_dir"/*.wants "$runtime_unit_dir"/*.requires; do
        rm -f -- "$link_dir/$app_name.service"
    done
    rm -f -- "$policy_lua_dir/90-$app_name-phone.lua" "$policy_conf_dir/90-$app_name-phone.conf"
    for temporary in "$policy_lua_dir/.$app_name-"* "$policy_conf_dir/.$app_name-"*; do
        if [ -e "$temporary" ]; then rm -- "$temporary"; fi
    done
    rm -f -- "$bin_dir/$app_name" "$bin_dir/${app_name}d" "$bin_dir/$app_name-phone-policy" "$unit_dir/$app_name.service"
    rm -rf --one-file-system -- "$config_dir" "$data_dir" "$state_dir" "$cache_dir" "$unit_dir/$app_name.service.d" "$runtime_unit_dir/$app_name.service.d"
done
systemctl --user daemon-reload

for app_name in $app_names; do
    select_app "$app_name"
    for item in "$runtime_dir"/* "$runtime_dir"/.[!.]* "$runtime_dir"/..?*; do
        if [ "$item" = "$runtime_dir/controller.lock" ] || { [ ! -e "$item" ] && [ ! -L "$item" ]; }; then continue; fi
        rm -rf --one-file-system -- "$item"
    done
    case "$app_name" in
        bluetooth-audio-bridge) lock_identity=$current_lock_identity ;;
        bt-audio-bridge) lock_identity=$legacy_lock_identity ;;
    esac
    if [ "$lock_identity" != "$(stat -c '%d:%i' "$runtime_dir/controller.lock")" ]; then
        printf '%s\n' "The $app_name runtime lock was replaced. Cleanup is incomplete; close other bridge commands and retry." >&2
        exit 1
    fi
    rm -- "$runtime_dir/controller.lock"
    if ! rmdir -- "$runtime_dir"; then
        printf '%s\n' "Cannot remove $runtime_dir. Cleanup is incomplete; close other bridge commands and retry." >&2
        exit 1
    fi
done

# ลบ parent ที่ตัวติดตั้งอาจสร้างไว้เฉพาะเมื่อว่าง โดยเก็บ shared configuration อื่นไว้
for directory in "$unit_dir/default.target.wants" "$runtime_unit_dir/default.target.wants" "$policy_lua_dir" "$policy_conf_dir" "$config_home/wireplumber"; do
    rmdir -- "$directory" 2>/dev/null || :
done

printf '%s\n' 'Removed the current bluetooth-audio-bridge installation and managed legacy bt-audio-bridge files.'
printf '%s\n' 'Removed binaries, user units and overrides, autostart links, generated WirePlumber policies and temporary files, and project XDG config/data/state/cache/runtime directories (including sockets and locks).'
printf '%s\n' 'The user service manager was refreshed. No running service was stopped or restarted.'
printf '%s\n' 'Ubuntu packages, Bluetooth pairings, other applications, shared audio state and the source/build directory remain available.'
printf '%s\n' 'Log out and back in to unload the removed policy from WirePlumber and restore Ubuntu automatic Bluetooth playback. Standard make install will not add that policy again.'
