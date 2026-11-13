#!/bin/sh
# One-line install of rfscan on Linux with apt (Debian, Ubuntu, Raspberry Pi OS):
#
#   curl -fsSL https://raw.githubusercontent.com/b4sh/rfscan/main/packaging/linux/get.sh | sh
#   ... | sh -s -- --main      the development version (main) without asking
#
# It first asks whether to install the latest release or the development version, prints what
# it will install and waits for a confirmation; nothing is changed before.
# Before that it asks which SoapySDR modules (receiver drivers) to install; SDRplay is built from
# source when the SDRplay API is installed. Then it installs the missing build packages and the
# modules (apt) and Rust (rustup, into ~/.cargo), keeps the sources in /usr/local/src/rfscan
# (owned by the user who runs it, synchronized on every run), builds and installs rfscan with
# packaging/linux/install.sh, asks whether to install the systemd service and removes the build
# files. Run it again to update.

set -eu

REPO=https://github.com/b4sh/rfscan
API=https://api.github.com/repos/b4sh/rfscan
SRC=/usr/local/src/rfscan
UNIT=/etc/systemd/system/rfscan.service
# The list of APT in install.sh without the module packages: the modules are chosen.
PACKAGES="build-essential pkg-config clang libclang-dev git libsoapysdr-dev libasound2-dev"
# SDRplay: the module is not in apt and is built from source when the SDRplay API is installed.
PLAY_REPO=https://github.com/pothosware/SoapySDRPlay3
PLAY_SRC=/usr/local/src/SoapySDRPlay3
PLAY_API=/usr/local/lib/libsdrplay_api.so
PLAY_MODULE=/usr/local/lib/SoapySDR/modules0.8/libsdrPlaySupport.so

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

installed_pkg() {
    dpkg-query -W -f='${Status}\n' "$1" 2>/dev/null | grep -q 'install ok installed'
}

# The SoapySDR modules in apt, one "name|description" line each, without all (every module) and
# audio (a sound card, not a receiver). The packages independent of the SoapySDR ABI version.
list_modules() {
    apt-cache search --names-only '^soapysdr-module-' 2>/dev/null |
        sed -n 's/^soapysdr-module-\([^ ]*\) - \(.*\)$/\1|\2/p' |
        sed 's/ (default[^)]*)$//' | grep -v '^\(all\|audio\)|' | sort
}

# An installed module: its package of any SoapySDR ABI version.
installed_module() {
    dpkg-query -W -f='${Status}\n' "soapysdr*-module-$1" 2>/dev/null | grep -q 'install ok installed'
}

# Asks which modules to install. Sets modules (names) and sdrplay (0/1).
choose_modules() {
    mods="$(list_modules)"
    [ -n "$mods" ] || die "no SoapySDR modules (soapysdr-module-*) in apt: run sudo apt-get update and try again"
    count="$(printf '%s\n' "$mods" | wc -l)"
    play=$((count + 1))
    echo "SoapySDR modules (receiver drivers):"
    i=0
    printf '%s\n' "$mods" | while IFS='|' read -r name desc; do
        i=$((i + 1))
        mark=""
        ! installed_module "$name" || mark=" [installed]"
        [ "$name" != rtlsdr ] || mark="$mark [default]"
        printf '  %2d) %-10s %s%s\n' "$i" "$name" "$desc" "$mark"
    done
    mark=""
    [ ! -e "$PLAY_MODULE" ] || mark=" [installed]"
    [ -e "$PLAY_API" ] || mark="$mark [needs the SDRplay API first: https://www.sdrplay.com/api/]"
    printf '  %2d) %-10s %s%s\n' "$play" sdrplay "SDRplay RSP devices (SoapySDRPlay3, built from source)" "$mark"
    while :; do
        printf 'Modules to install (numbers separated by spaces, none, Enter: rtlsdr): ' >/dev/tty
        answer=""
        read -r answer </dev/tty || answer=""
        modules=""
        sdrplay=0
        ok=1
        if [ -z "$answer" ]; then
            if printf '%s\n' "$mods" | grep -q '^rtlsdr|'; then
                modules=rtlsdr
                break
            fi
            echo "rtlsdr is not in apt: choose by number" >/dev/tty
            continue
        fi
        for w in $answer; do
            case "$w" in
                none) [ "$answer" = none ] || ok=0 ;;
                *[!0-9]*) ok=0 ;;
                *)
                    if [ "$w" -ge 1 ] && [ "$w" -le "$count" ]; then
                        name="$(printf '%s\n' "$mods" | sed -n "${w}p" | cut -d'|' -f1)"
                        case " $modules " in
                            *" $name "*) ;;
                            *) modules="${modules:+$modules }$name" ;;
                        esac
                    elif [ "$w" -eq "$play" ]; then
                        if [ -e "$PLAY_API" ]; then
                            sdrplay=1
                        else
                            echo "SDRplay needs the SDRplay API ($PLAY_API): install it from" >/dev/tty
                            echo "https://www.sdrplay.com/api/ first (see DOC.md, SDRplay on Linux)" >/dev/tty
                            ok=0
                        fi
                    else
                        ok=0
                    fi
                    ;;
            esac
        done
        [ "$ok" -eq 0 ] || break
        echo "answer with numbers from 1 to $play separated by spaces, none, or Enter" >/dev/tty
    done
}

# The short commit of main, through git or, without git, the GitHub API.
main_commit() {
    if command -v git >/dev/null 2>&1; then
        git ls-remote "$REPO" refs/heads/main | cut -c1-7
    else
        curl -fsSL "$API/commits/main" | sed -n 's/^  "sha": *"\([0-9a-f]\{7\}\).*/\1/p' | head -n 1
    fi
}

# Asks whether to install the latest release (tag) or the development version (sets use_main).
choose_version() {
    commit="$(main_commit)" || commit=""
    echo "Version to install:"
    echo "  1) $tag (the latest release) [default]"
    echo "  2) the development version (main${commit:+, $commit}): the newest changes, may be unfinished"
    while :; do
        printf 'Version (1 or 2, Enter: 1): ' >/dev/tty
        answer=""
        read -r answer </dev/tty || answer=""
        case "$answer" in
            "" | 1) return ;;
            2) use_main=1; return ;;
            *) echo "answer 1, 2 or Enter" >/dev/tty ;;
        esac
    done
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
                echo "usage: get.sh [--main]   (--main: the development version, without asking)"
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

    if [ "$use_main" -eq 0 ]; then
        tag="$(latest_tag)" || tag=""
        [ -n "$tag" ] || die "cannot find the latest release of $REPO (no network?)"
        choose_version
        echo
    fi
    choose_modules
    echo

    missing=""
    for p in $PACKAGES; do
        installed_pkg "$p" || missing="$missing $p"
    done
    new_modules=""
    for m in $modules; do
        installed_module "$m" || new_modules="$new_modules soapysdr-module-$m"
    done
    missing="$missing$new_modules"
    if [ "$sdrplay" -eq 1 ]; then
        installed_pkg cmake || missing="$missing cmake"
    fi
    missing="${missing# }"

    rust=1
    if command -v cargo >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]; then
        rust=0
    fi

    if [ "$use_main" -eq 1 ]; then
        ref=origin/main
        target_name="the development version (main)"
    else
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

    play_state=new
    chown_play=0
    if [ "$sdrplay" -eq 1 ] && [ -e "$PLAY_SRC" ]; then
        if [ -d "$PLAY_SRC/.git" ] && grep -qsE "url = (https://github\.com/|git@github\.com:)pothosware/SoapySDRPlay3(\.git)?/?\$" "$PLAY_SRC/.git/config"; then
            play_state=update
            [ "$(stat -c %U "$PLAY_SRC")" = "$user" ] || chown_play=1
        else
            die "$PLAY_SRC exists and is not a clone of $PLAY_REPO: move it away and run the installer again"
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
    if [ -n "$modules" ]; then
        echo "  - SoapySDR modules: $modules${new_modules:+ (apt:$new_modules)}"
    else
        echo "  - SoapySDR modules from apt: none chosen"
    fi
    if [ "$sdrplay" -eq 1 ]; then
        if [ "$play_state" = new ]; then
            echo "  - clone SoapySDRPlay3 into $PLAY_SRC (created with sudo, owned by $user),"
        else
            echo "  - update SoapySDRPlay3 in $PLAY_SRC (local changes in it are discarded),"
        fi
        [ "$chown_play" -eq 0 ] || echo "    give it to $user (sudo chown -R; now owned by $(stat -c %U "$PLAY_SRC")),"
        echo "    build the SDRplay module and install it to $(dirname "$PLAY_MODULE") (sudo)"
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
        # Without the recommended packages: libsoapysdr recommends every SoapySDR module.
        # shellcheck disable=SC2086 # a list of package names
        sudo apt-get install -y --no-install-recommends $missing
    fi
    if [ "$rust" -eq 1 ]; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    fi
    case ":$PATH:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) [ ! -d "$HOME/.cargo/bin" ] || PATH="$HOME/.cargo/bin:$PATH" ;;
    esac
    export PATH

    # SDRplay module.
    if [ "$sdrplay" -eq 1 ]; then
        if [ "$play_state" = new ]; then
            sudo mkdir -p "$PLAY_SRC"
            sudo chown "$user:$group" "$PLAY_SRC"
            git clone "$PLAY_REPO" "$PLAY_SRC"
        elif [ "$chown_play" -eq 1 ]; then
            sudo chown -R "$user:$group" "$PLAY_SRC"
        fi
        git -C "$PLAY_SRC" fetch --force origin
        git -C "$PLAY_SRC" remote set-head origin --auto >/dev/null
        git -C "$PLAY_SRC" checkout --quiet --force --detach origin/HEAD
        git -C "$PLAY_SRC" clean -fdxq
        cmake -S "$PLAY_SRC" -B "$PLAY_SRC/build"
        cmake --build "$PLAY_SRC/build" -j "$(nproc)"
        sudo cmake --install "$PLAY_SRC/build"
        sudo ldconfig
        # sudo: the install leaves a root-owned install_manifest.txt in it.
        sudo rm -rf "$PLAY_SRC/build"
        echo "installed the SDRplay module $PLAY_MODULE ($(git -C "$PLAY_SRC" describe --tags --always))"
    fi

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
