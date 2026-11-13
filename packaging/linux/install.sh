#!/bin/sh
# Builds rfscan from this source tree and installs it (Linux, including the Raspberry Pi).
#
#   packaging/linux/install.sh               build as the current user, install with sudo
#   packaging/linux/install.sh --no-build    install target/release/rfscan as it is
#   packaging/linux/install.sh --service     also install and start the systemd service
#                                            (with --no-build and no build: only the service,
#                                            for the installed $PREFIX/bin/rfscan)
#   packaging/linux/install.sh --uninstall   remove the installed files and the service
#
# Files: $PREFIX/bin/rfscan and $PREFIX/share/doc/rfscan/ (README.md, DOC.md, LICENSE).
# --service: /etc/systemd/system/rfscan.service, running rfscan as the installing user with
# that user's ~/.config/rfscan/rfscan.conf, started at boot; log: journalctl -u rfscan.
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
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

build=1
uninstall=0
service=0
UNIT=/etc/systemd/system/rfscan.service
for arg in "$@"; do
    case "$arg" in
        --no-build) build=0 ;;
        --uninstall) uninstall=1 ;;
        --service) service=1 ;;
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

# sudo unless already root (the service files are always in /etc).
asroot() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

if [ "$uninstall" -eq 1 ]; then
    if [ -e "$UNIT" ]; then
        asroot systemctl disable --now rfscan.service || true
        asroot rm -f "$UNIT"
        asroot systemctl daemon-reload
        echo "removed the service $UNIT"
    fi
    run rm -f "$BIN"
    run rm -rf "$DOCDIR"
    echo "removed $BIN and $DOCDIR (~/.config/rfscan is kept)"
    exit 0
fi

# The service runs as the user who installs it (with sudo: the user who called sudo).
if [ "$service" -eq 1 ]; then
    if ! command -v systemctl >/dev/null 2>&1; then
        echo "--service needs systemd (systemctl not found)" >&2
        exit 1
    fi
    if [ "$(id -u)" -eq 0 ]; then
        svc_user="${SUDO_USER:-}"
        if [ -z "$svc_user" ] || [ "$svc_user" = root ]; then
            echo "--service: run the script as the user the service should run as" >&2
            exit 1
        fi
    else
        svc_user="$(id -un)"
    fi
    if [ ! -f "$ROOT/packaging/linux/rfscan.service.in" ]; then
        echo "$ROOT/packaging/linux/rfscan.service.in not found" >&2
        exit 1
    fi
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

# --service --no-build without a build (e.g. after the one-line install removed it): only the
# service, for the program already installed.
service_only=0
if [ ! -x "$ROOT/target/release/rfscan" ]; then
    if [ "$build" -eq 0 ] && [ "$service" -eq 1 ] && [ -x "$BIN" ]; then
        service_only=1
    else
        echo "$ROOT/target/release/rfscan not found: build it first (without --no-build)" >&2
        exit 1
    fi
fi

if [ "$service_only" -eq 0 ]; then
    run install -Dm755 "$ROOT/target/release/rfscan" "$BIN"
    run install -Dm644 -t "$DOCDIR" "$ROOT/README.md" "$ROOT/DOC.md" "$ROOT/LICENSE"
    echo "installed $BIN ($("$BIN" --version)) and $DOCDIR"
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) echo "note: $PREFIX/bin is not in PATH" ;;
    esac
else
    echo "no build in $ROOT/target: installing only the service for $BIN ($("$BIN" --version))"
fi

if [ "$service" -eq 1 ]; then
    tmp="$(mktemp)"
    sed -e "s|@USER@|$svc_user|g" -e "s|@BIN@|$BIN|g" "$ROOT/packaging/linux/rfscan.service.in" >"$tmp"
    asroot install -Dm644 "$tmp" "$UNIT"
    rm -f "$tmp"
    asroot systemctl daemon-reload
    asroot systemctl enable rfscan.service
    asroot systemctl restart rfscan.service
    home="$(getent passwd "$svc_user" | cut -d: -f6)"
    conf="$home/.config/rfscan/rfscan.conf"
    cat <<EOF

installed and started the service $UNIT (user $svc_user)
  log and the web GUI link:  journalctl -u rfscan -f
  stop / start / restart:    sudo systemctl stop|start|restart rfscan
  settings:                  $conf (restart the service after a change)
EOF
    if ! grep -qs '^[[:space:]]*web-gui[[:space:]]*=' "$conf"; then
        cat <<EOF
note: the web GUI listens on 127.0.0.1:7373 only (this computer). For a phone in the home
network add to $conf:

  web-gui = "0.0.0.0:7373"

and run: sudo systemctl restart rfscan
EOF
    fi
fi

# RTL-SDR: the kernel's DVB-T driver takes the receiver when it is plugged in. Only a warning
# with the commands; the script changes no system settings.
DVB=dvb_usb_rtl28xxu
loaded=0
blocked=0
grep -qs "^$DVB " /proc/modules && loaded=1
grep -qsE "^[[:space:]]*(blacklist[[:space:]]+$DVB|install[[:space:]]+$DVB[[:space:]]+/bin/(false|true))" \
    /etc/modprobe.d/*.conf /usr/lib/modprobe.d/*.conf /lib/modprobe.d/*.conf && blocked=1
if [ "$blocked" -eq 0 ] && { [ "$loaded" -eq 1 ] || PATH="$PATH:/sbin:/usr/sbin" modinfo "$DVB" >/dev/null 2>&1; }; then
    cat <<EOF

note for RTL-SDR receivers (not needed for Airspy or SDRplay):
the kernel's DVB-T driver ($DVB) is not blocked. It takes the receiver when it is
plugged in; librtlsdr detaches it when rfscan opens the receiver, but after a few runs the
stream may stop ("the stream delivers no samples") or the receiver may not open at all.
Block the driver once: unplug the receiver, run

  printf 'blacklist $DVB\nblacklist rtl2832_sdr\nblacklist rtl2832\n' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf
  sudo modprobe -r $DVB rtl2832_sdr rtl2832

and plug it in again (see DOC.md, Installation).
EOF
elif [ "$loaded" -eq 1 ]; then
    cat <<EOF

note for RTL-SDR receivers: the DVB-T driver ($DVB) is blocked but still loaded.
Unplug the receiver, run

  sudo modprobe -r $DVB rtl2832_sdr rtl2832

and plug it in again.
EOF
fi
