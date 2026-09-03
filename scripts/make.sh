#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
action=${1:-help}
if [ "$#" -gt 0 ]; then
    shift
fi

if [ "$action" = help ]; then
    cat <<'USAGE'
Bluetooth Audio Bridge

  make build                         Build release binaries
  make install                       Build and install for your desktop user
  make devices                       List Bluetooth devices
  make select IPHONE=MAC HEADPHONES=MAC
  make config                        Show saved configuration
  make phone-policy IPHONE=MAC        Preview the phone input rule
  make phone-policy-install IPHONE=MAC
  make run                           Run the installed daemon in the foreground
  make status                        Show bridge status
  make volume CHANNEL=phone VALUE=0.4
  make mute CHANNEL=phone STATE=on
  make enable                        Enable mixing
  make disable                       Disable mixing
  make uninstall                     Remove this user's bridge installation

CHANNEL: phone, desktop, master. VALUE: 0.0-1.0. STATE: on, off.
sudo make ... is supported and runs as the invoking desktop user.
Installation does not start services. See README.md for phone setup/removal.
USAGE
    exit 0
fi

if [ "$(id -u)" -eq 0 ]; then
    if [ -z "${SUDO_USER:-}" ] || [ "${SUDO_UID:-0}" = 0 ] ||
        [ "$(id -u "$SUDO_USER" 2>/dev/null)" != "${SUDO_UID:-}" ]; then
        printf '%s\n' 'Run make from your desktop account, optionally using sudo make.' >&2
        exit 1
    fi
    app_home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
    case "$app_home" in
        /*) ;;
        *) printf '%s\n' 'Cannot find the invoking user home directory.' >&2; exit 1 ;;
    esac
    app_runtime=${XDG_RUNTIME_DIR:-/run/user/$SUDO_UID}
    exec runuser -u "$SUDO_USER" -- env \
        HOME="$app_home" \
        PATH="${CARGO_HOME:-$app_home/.cargo}/bin:$app_home/.local/bin:$PATH" \
        CARGO_HOME="${CARGO_HOME:-$app_home/.cargo}" \
        RUSTUP_HOME="${RUSTUP_HOME:-$app_home/.rustup}" \
        XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$app_home/.config}" \
        XDG_DATA_HOME="${XDG_DATA_HOME:-$app_home/.local/share}" \
        XDG_STATE_HOME="${XDG_STATE_HOME:-$app_home/.local/state}" \
        XDG_CACHE_HOME="${XDG_CACHE_HOME:-$app_home/.cache}" \
        XDG_RUNTIME_DIR="$app_runtime" \
        DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$app_runtime/bus}" \
        "$project_dir/scripts/make.sh" "$action" "$@"
fi

app_program="$HOME/.local/bin/bluetooth-audio-bridge"
case "$action" in
    build|install|uninstall)
        exec "$project_dir/scripts/$action.sh" "$@"
        ;;
    phone-policy|phone-policy-install)
        : "${IPHONE:?Usage: make phone-policy[-install] IPHONE=AA:BB:CC:DD:EE:FF}"
        if [ "$action" = phone-policy-install ]; then
            exec "$project_dir/scripts/phone-policy.sh" "$IPHONE" --install "$@"
        fi
        exec "$project_dir/scripts/phone-policy.sh" "$IPHONE" "$@"
        ;;
    run)
        app_program="$HOME/.local/bin/bluetooth-audio-bridged"
        ;;
    devices|status|enable|disable)
        set -- "$action" "$@"
        ;;
    config)
        set -- config show "$@"
        ;;
    select)
        : "${IPHONE:?Usage: make select IPHONE=MAC HEADPHONES=MAC}"
        : "${HEADPHONES:?Usage: make select IPHONE=MAC HEADPHONES=MAC}"
        set -- select --iphone "$IPHONE" --headphones "$HEADPHONES" "$@"
        ;;
    volume)
        : "${CHANNEL:?Usage: make volume CHANNEL=phone|desktop|master VALUE=0.0-1.0}"
        : "${VALUE:?Usage: make volume CHANNEL=phone|desktop|master VALUE=0.0-1.0}"
        set -- volume "$CHANNEL" "$VALUE" "$@"
        ;;
    mute)
        : "${CHANNEL:?Usage: make mute CHANNEL=phone|desktop|master STATE=on|off}"
        : "${STATE:?Usage: make mute CHANNEL=phone|desktop|master STATE=on|off}"
        set -- mute "$CHANNEL" "$STATE" "$@"
        ;;
    *)
        printf '%s\n' "Unknown action: $action. Run make help." >&2
        exit 2
        ;;
esac

if [ ! -x "$app_program" ]; then
    printf '%s\n' "Missing installed program: $app_program. Run make install first." >&2
    exit 1
fi
exec "$app_program" "$@"
