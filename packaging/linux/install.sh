#!/bin/sh
# Builds rfscan from this source tree and installs it (Linux, including the Raspberry Pi).
#
#   packaging/linux/install.sh               build as the current user, install with sudo
#   packaging/linux/install.sh --no-build    install target/release/rfscan as it is
#   packaging/linux/install.sh --uninstall   remove the installed files
#
# Files: $PREFIX/bin/rfscan and $PREFIX/share/doc/rfscan/ (README.md, DOC.md, LICENSE).
# PREFIX defaults to /usr/local, e.g. PREFIX=/opt/rfscan packaging/linux/install.sh.
# The user's files in ~/.config/rfscan are never touched.
#
# The script installs no system packages: when a build dependency is missing it prints the
# apt command (Debian, Ubuntu, Raspberry Pi OS) and stops.

set -eu

PREFIX="${PREFIX:-/usr/local}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$PREFIX/bin/rfscan"
DOCDIR="$PREFIX/share/doc/rfscan"
APT="sudo apt install build-essential pkg-config clang libclang-dev git libsoapysdr-dev soapysdr-module-rtlsdr soapysdr-module-airspy libasound2-dev"

usage() {
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
}

build=1
uninstall=0
for arg in "$@"; do
    case "$arg" in
        --no-build) build=0 ;;
        --uninstall) uninstall=1 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

# sudo only when the target directory is not writable by the current user.
run() {
    dir="$PREFIX"
    while [ ! -d "$dir" ]; do dir="$(dirname "$dir")"; done
    if [ -w "$dir" ] && { [ ! -e "$PREFIX/bin" ] || [ -w "$PREFIX/bin" ]; }; then
        "$@"
    else
        sudo "$@"
    fi
}

if [ "$uninstall" -eq 1 ]; then
    run rm -f "$BIN"
    run rm -rf "$DOCDIR"
    echo "removed $BIN and $DOCDIR (~/.config/rfscan is kept)"
    exit 0
fi

if [ "$build" -eq 1 ]; then
    # cargo as root would leave a root-owned target/ and usually has no Rust in PATH.
    if [ "$(id -u)" -eq 0 ]; then
        echo "run the script as a normal user (it asks for sudo only to install)," >&2
        echo "or build first and run it with --no-build" >&2
        exit 1
    fi
    missing=""
    command -v cargo >/dev/null 2>&1 || missing="$missing cargo"
    command -v cc >/dev/null 2>&1 || missing="$missing cc"
    command -v clang >/dev/null 2>&1 || missing="$missing clang"
    if command -v pkg-config >/dev/null 2>&1; then
        pkg-config --exists SoapySDR || missing="$missing SoapySDR"
        pkg-config --exists alsa || missing="$missing ALSA"
    else
        missing="$missing pkg-config"
    fi
    if [ -n "$missing" ]; then
        echo "missing:$missing" >&2
        echo "install the build dependencies:" >&2
        echo "  $APT" >&2
        command -v cargo >/dev/null 2>&1 || echo "and Rust: https://rustup.rs/" >&2
        exit 1
    fi
    (cd "$ROOT" && cargo build --release --locked)
fi

if [ ! -x "$ROOT/target/release/rfscan" ]; then
    echo "$ROOT/target/release/rfscan not found: build it first (without --no-build)" >&2
    exit 1
fi

run install -Dm755 "$ROOT/target/release/rfscan" "$BIN"
run install -Dm644 -t "$DOCDIR" "$ROOT/README.md" "$ROOT/DOC.md" "$ROOT/LICENSE"
echo "installed $BIN ($("$BIN" --version)) and $DOCDIR"
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "note: $PREFIX/bin is not in PATH" ;;
esac
