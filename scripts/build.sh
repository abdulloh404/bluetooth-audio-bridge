#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ "$(id -u)" -eq 0 ]; then
    exec "$project_dir/scripts/make.sh" build "$@"
fi

if ! command -v cargo >/dev/null 2>&1; then
    PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
    export PATH
fi
if ! command -v cargo >/dev/null 2>&1; then
    printf '%s\n' 'Cargo was not found. Install Rust/Cargo for your desktop user or set CARGO_HOME.' >&2
    exit 127
fi

cd "$project_dir"
exec cargo build --workspace --release "$@"
