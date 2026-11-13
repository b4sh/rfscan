#!/bin/sh
# One-line install of rfscan on Linux with apt (Debian, Ubuntu, Raspberry Pi OS):
#
#   curl -fsSL https://raw.githubusercontent.com/b4sh/rfscan/main/packaging/linux/get.sh | sh
#   ... | sh -s -- --main      the development version (main) instead of the latest release
#
# It first prints what it will install and waits for a confirmation; nothing is changed before.
# Then it installs the missing build packages (apt) and Rust (rustup, into ~/.cargo), keeps the
# sources in /usr/local/src/rfscan (owned by the user who runs it, synchronized on every run),
# builds and installs rfscan with packaging/linux/install.sh, asks whether to install the systemd
# service and removes the build files. Run it again to update.

set -eu

REPO=https://github.com/b4sh/rfscan
API=https://api.github.com/repos/b4sh/rfscan
SRC=/usr/local/src/rfscan
UNIT=/etc/systemd/system/rfscan.service
# The same list as APT in install.sh.
PACKAGES="build-essential pkg-config clang libclang-dev git libsoapysdr-dev soapysdr-module-rtlsdr soapysdr-module-airspy libasound2-dev"

die() {
    echo "error: $*" >&2
    exit 1
}

# The answers come from the terminal: the script's standard input is the pipe from curl.
ask() {
    printf '%s ' "$1" >/dev/tty
    answer=""
    read -r answer </dev/tty || answer=""
    case "$answer" in
        y | Y | yes | YES | Yes) return 0 ;;
        *) return 1 ;;
    esac
}

# The latest release tag (vX.Y.Z), through git or, without git, the GitHub API.
latest_tag() {
    if command -v git >/dev/null 2>&1; then
        git ls-remote --tags --refs "$REPO" 'v*' | sed -n 's|.*refs/tags/\(v[0-9][0-9.]*\)$|\1|p' |
            sort -t. -k1.2,1n -k2,2n -k3,3n | tail -n 1
    else
        curl -fsSL "$API/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1
    fi
}

main() {
    use_main=0
    for arg in "$@"; do
        case "$arg" in
            --main) use_main=1 ;;
            -h | --help)
                echo "usage: get.sh [--main]   (--main: the development version instead of the latest release)"
                exit 0
                ;;
            *) die "unknown option: $arg (only --main)" ;;
        esac
    done

    # Read-only checks.
    [ "$(uname -s)" = Linux ] || die "this installer is for Linux"
    [ "$(id -u)" -ne 0 ] || die "run it as a normal user, not as root (it asks for sudo where needed)"
    (: </dev/tty) 2>/dev/null || die "no terminal to ask in: run the command in a terminal"
    command -v apt-get >/dev/null 2>&1 && command -v dpkg-query >/dev/null 2>&1 ||
        die "apt-get not found: only Debian, Ubuntu and Raspberry Pi OS are supported. On other systems install SoapySDR, ALSA, clang, pkg-config, git and Rust, clone $REPO and run packaging/linux/install.sh"
    command -v sudo >/dev/null 2>&1 || die "sudo not found"
    user="$(id -un)"
    group="$(id -gn)"

    missing=""
    for p in $PACKAGES; do
        dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' ||
            missing="$missing $p"
    done
    missing="${missing# }"

    rust=1
    if command -v cargo >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]; then
        rust=0
    fi

    if [ "$use_main" -eq 1 ]; then
        ref=origin/main
        target_name="the development version (main)"
    else
        tag="$(latest_tag)" || tag=""
        [ -n "$tag" ] || die "cannot find the latest release of $REPO (no network?)"
        ref="$tag"
        target_name="$tag"
    fi

    installed=""
    if command -v rfscan >/dev/null 2>&1; then
        installed="$(rfscan --version 2>/dev/null | head -n 1)" || installed=""
    fi

    src_state=new
    src_version=""
    chown_src=0
    if [ -e "$SRC" ]; then
        if [ -d "$SRC/.git" ] && grep -qsE "url = (https://github\.com/|git@github\.com:)b4sh/rfscan(\.git)?/?\$" "$SRC/.git/config"; then
            src_state=update
            src_version="$(git -c safe.directory="$SRC" -C "$SRC" describe --tags --always 2>/dev/null)" ||
                src_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" 2>/dev/null | head -n 1)"
            [ "$(stat -c %U "$SRC")" = "$user" ] || chown_src=1
        else
            die "$SRC exists and is not a clone of $REPO: move it away and run the installer again"
        fi
    fi

    service_state=none
    if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
        service_state=absent
        if [ -e "$UNIT" ]; then
            if systemctl is-enabled --quiet rfscan.service 2>/dev/null; then
                service_state=enabled
            else
                service_state=disabled
            fi
        fi
    fi

    # The summary.
    echo "rfscan installer: $target_name from $REPO"
    [ -z "$installed" ] || echo "installed now: $installed"
    echo
    echo "This will:"
    if [ -n "$missing" ]; then
        echo "  - install system packages (sudo apt-get install): $missing"
    else
        echo "  - system packages: all present"
    fi
    if [ "$rust" -eq 1 ]; then
        echo "  - install Rust with rustup into $HOME/.cargo (minimal profile; rustup adds"
        echo "    $HOME/.cargo/bin to PATH in your shell profile)"
    else
        echo "  - Rust: present"
    fi
    if [ "$src_state" = new ]; then
        echo "  - clone the sources into $SRC (created with sudo, owned by $user)"
    else
        echo "  - update the sources in $SRC from ${src_version:-an unknown version} to $target_name"
        echo "    (local changes in it are discarded)"
    fi
    [ "$chown_src" -eq 0 ] || echo "  - give $SRC to $user (sudo chown -R; now owned by $(stat -c %U "$SRC"))"
    echo "  - build rfscan as $user (about 7-8 minutes on a Raspberry Pi 4) and install"
    echo "    /usr/local/bin/rfscan and /usr/local/share/doc/rfscan (sudo)"
    echo "  - remove the build files afterwards (cargo clean)"
    case "$service_state" in
        enabled) echo "  - restart the enabled systemd service rfscan with the new version" ;;
        absent) echo "  - at the end ask whether to install the systemd service" ;;
        disabled) echo "  - leave the installed but not enabled systemd service rfscan as it is" ;;
        *) ;;
    esac
    echo "sudo asks for your password when needed."
    echo
    if ! ask "Continue? [y/N]"; then
        echo "nothing was changed"
        exit 0
    fi

    # Dependencies.
    if [ -n "$missing" ]; then
        sudo apt-get update
        # shellcheck disable=SC2086 # a list of package names
        sudo apt-get install -y $missing
    fi
    if [ "$rust" -eq 1 ]; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    fi
    case ":$PATH:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) [ ! -d "$HOME/.cargo/bin" ] || PATH="$HOME/.cargo/bin:$PATH" ;;
    esac
    export PATH

    # Sources.
    if [ "$src_state" = new ]; then
        sudo mkdir -p "$SRC"
        sudo chown "$user:$group" "$SRC"
        git clone "$REPO" "$SRC"
    elif [ "$chown_src" -eq 1 ]; then
        sudo chown -R "$user:$group" "$SRC"
    fi
    git -C "$SRC" fetch --tags --force origin
    git -C "$SRC" checkout --quiet --force --detach "$ref"
    git -C "$SRC" clean -fdxq
    echo "sources: $SRC at $(git -C "$SRC" describe --tags --always)"

    # Build and install (the files go to /usr/local whatever PREFIX the environment has).
    PREFIX=/usr/local "$SRC/packaging/linux/install.sh"

    # The service.
    case "$service_state" in
        enabled)
            sudo systemctl restart rfscan.service
            echo "restarted the service rfscan (log: journalctl -u rfscan -f)"
            ;;
        disabled)
            echo
            echo "the systemd service rfscan is installed but not enabled; left as it is."
            echo "To start it now and at boot: sudo systemctl enable --now rfscan"
            ;;
        absent)
            echo
            if ask "Install the systemd service (starts rfscan at boot)? [y/N]"; then
                PREFIX=/usr/local "$SRC/packaging/linux/install.sh" --no-build --service
            else
                cat <<EOF
The service was not installed. To install it later (rfscan then runs as $user at boot,
with the settings of ~/.config/rfscan/rfscan.conf):

  $SRC/packaging/linux/install.sh --no-build --service

See https://github.com/b4sh/rfscan/blob/main/DOC.md#running-rfscan-as-a-server
EOF
            fi
            ;;
        *) ;;
    esac

    # The build files stay only when something above failed.
    (cd "$SRC" && cargo clean --quiet)
    echo "removed the build files ($SRC/target)"
    echo
    echo "done: $(/usr/local/bin/rfscan --version)"
}

# The whole script is read before anything runs (curl | sh).
main "$@"
