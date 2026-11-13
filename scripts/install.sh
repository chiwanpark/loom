#!/bin/sh
# Installs Loom's agent or relay.
#
# By default it downloads the statically linked binary from the latest GitHub
# release, checks it against the release's SHA256SUMS, installs it, and writes
# a configuration if there is none. Running it again upgrades it.
#
#   curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s agent
#   curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sudo sh -s relay --systemd
#   ssh devbox sh -s agent < scripts/install.sh      # on a host you reach over SSH
#
# Run with --help for the options.
set -eu

REPO_URL=https://github.com/chiwanpark/loom
# Where the releases and the repository's files are, and the directory the
# relay's system-wide paths are under. Tests point these at local copies.
RELEASES_URL=${LOOM_RELEASES_URL:-$REPO_URL/releases}
RAW_URL=${LOOM_RAW_URL:-https://raw.githubusercontent.com/chiwanpark/loom}
SYSROOT=${LOOM_INSTALL_SYSROOT:-}
TARGET=x86_64-unknown-linux-musl

usage() {
    cat <<EOF
Installs Loom's agent or relay.

Usage: install.sh agent|relay [options]

  agent  loom-agent, for the current user: in ~/.local/bin, with its
         configuration in ~/.config/loom/agent.yaml.
  relay  loom-relay. As root it is installed for the whole system: in
         /usr/local/bin, with its configuration in /etc/loom/relay.yaml.
         Otherwise it is installed for the current user, like the agent.

Options:
  --version <version>  Install this release, such as 0.2.0, instead of the latest.
  --bin-dir <dir>      Install into this directory instead.
  --source             Build it with Cargo instead of downloading it, for hosts
                       without a prebuilt binary.
  --systemd            Also install its systemd service and start it. The
                       agent's is a user service that serves
                       lifecycle.default_workspace. The relay's is a system
                       service, so it needs root.
  --no-config          Don't write a configuration when there is none.
  -h, --help           Show this help.

Prebuilt binaries are for x86-64 Linux. Agents and relays that are already
running keep the old version until they are restarted.
EOF
}

say() {
    printf '%s\n' "$*"
}

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "this needs $1, which is not installed"
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "this needs curl or wget to download $name"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        die "this needs sha256sum or shasum to check the download"
    fi
}

# Paths that go into a systemd unit as they are.
plain_path() {
    case $2 in
        *[!A-Za-z0-9/._+-]*) die "--systemd needs $1 without spaces or special characters: $2" ;;
    esac
}

# Downloads the release archive and leaves its binary in $work/bin and its
# systemd unit in $work/unit.
download() {
    if [ -n "$version" ]; then
        dir=$RELEASES_URL/download/v$version
    else
        dir=$RELEASES_URL/latest/download
    fi
    fetch "$dir/SHA256SUMS" "$work/SHA256SUMS" ||
        die "could not download $dir/SHA256SUMS; is there a published release${version:+ v$version}?"
    # SHA256SUMS names every archive of the release, which also gives the
    # version of the latest one.
    line=$(sed 's/ \*/  /' "$work/SHA256SUMS" |
        grep -E "^[0-9a-f]{64}  $name-[0-9][^ /]*-$TARGET\\.tar\\.gz\$" | head -n 1 || true)
    [ -n "$line" ] || die "the release${version:+ v$version} has no $name archive for $TARGET"
    expected=${line%% *}
    archive=${line##* }
    found=${archive#"$name"-}
    found=${found%-"$TARGET".tar.gz}
    if [ -n "$version" ] && [ "$found" != "$version" ]; then
        die "release v$version lists $name $found"
    fi
    version=$found
    say "Downloading $name $version"
    fetch "$dir/$archive" "$work/$archive" || die "could not download $dir/$archive"
    actual=$(sha256 "$work/$archive")
    [ "$actual" = "$expected" ] ||
        die "$archive does not match its checksum in SHA256SUMS (expected $expected, got $actual)"
    mkdir "$work/unpack"
    tar -xzf "$work/$archive" -C "$work/unpack"
    stem=$work/unpack/${archive%.tar.gz}
    [ -f "$stem/$name" ] || die "$archive has no $name"
    cp "$stem/$name" "$work/bin"
    if [ -f "$stem/$unit" ]; then
        cp "$stem/$unit" "$work/unit"
    fi
}

# Builds the binary with Cargo into $work/bin, and fetches the systemd unit
# from the same tag when it is wanted.
build() {
    need cargo
    # Cargo warns when the directory it installs into is not on PATH; this
    # one is only a step on the way to $bin_dir.
    path=$work/root/bin:$PATH
    if [ -n "$version" ]; then
        ref=v$version
        say "Building $name $version with Cargo"
        PATH=$path cargo install --locked --git "$REPO_URL" --tag "$ref" --root "$work/root" "$name"
    else
        ref=main
        say "Building $name from the main branch with Cargo"
        PATH=$path cargo install --locked --git "$REPO_URL" --branch main --root "$work/root" "$name"
    fi
    cp "$work/root/bin/$name" "$work/bin"
    if [ "$systemd" = yes ]; then
        fetch "$RAW_URL/$ref/contrib/systemd/$unit" "$work/unit" ||
            die "could not download the systemd unit from $RAW_URL/$ref"
    fi
}

# Writes the unit into the directory $1, with its ExecStart pointed at the
# installed binary. The rest of the arguments are sed expressions for more
# changes.
write_unit() {
    [ -f "$work/unit" ] || die "the release has no systemd unit for $name"
    dest=$1
    shift
    mkdir -p "$dest"
    sed -e "s|^ExecStart=[^ ]*/$name |ExecStart=$target |" "$@" "$work/unit" >"$work/unit.new"
    grep -q "^ExecStart=$target " "$work/unit.new" ||
        die "could not point the systemd unit at $target"
    mv "$work/unit.new" "$dest/$unit"
    say "Installed $dest/$unit"
}

# Starts the service, or restarts it so that it runs the new version.
# $1 is --user for a user service.
start_service() {
    if systemctl ${1:+"$1"} is-active --quiet "$unit"; then
        systemctl ${1:+"$1"} restart "$unit"
        say "Restarted $unit"
    else
        systemctl ${1:+"$1"} enable --now "$unit"
        say "Started $unit"
    fi
}

# The agent's user service, which serves lifecycle.default_workspace.
agent_service() {
    need systemctl
    plain_path "--bin-dir" "$bin_dir"
    write_unit "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    if ! systemctl --user daemon-reload; then
        say "systemd for your user is not reachable from this shell. In a login session, run:"
        say "  systemctl --user daemon-reload && systemctl --user enable --now $unit"
        return
    fi
    # The service fails without a default workspace.
    if ! grep -Eq '^[[:space:]]+default_workspace:[[:space:]]*[^[:space:]#]' "$config" 2>/dev/null; then
        say "The service serves lifecycle.default_workspace, which $config does not set."
        say "Set it, then start the service:"
        say "  systemctl --user enable --now $unit"
        return
    fi
    start_service --user
    if command -v loginctl >/dev/null 2>&1 &&
        [ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)" = no ]; then
        say "It runs while you are logged in. To keep it running after you log out:"
        say "  loginctl enable-linger"
    fi
}

# The relay's system service, pointed at the installed binary and the
# configuration.
relay_service() {
    need systemctl
    plain_path "--bin-dir" "$bin_dir"
    plain_path "the configuration path" "$config"
    write_unit "$SYSROOT/etc/systemd/system" -e "/^ExecStart=/s|--config [^ ]*|--config $config|"
    grep -q "^ExecStart=$target --config $config " "$SYSROOT/etc/systemd/system/$unit" ||
        die "could not point the systemd unit at $config"
    systemctl daemon-reload || die "could not reload systemd"
    start_service ""
}

main() {
    case ${1:-} in
        agent | relay) component=$1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        "") die "say what to install: install.sh agent|relay [options] (see --help)" ;;
        *) die "unknown component: $1; install.sh installs agent or relay (see --help)" ;;
    esac
    shift
    name=loom-$component
    unit=$name.service
    version=
    bin_dir=
    source=no
    systemd=no
    write_config=yes
    while [ $# -gt 0 ]; do
        case $1 in
            --version)
                [ $# -ge 2 ] || die "--version needs a value"
                version=$2
                shift
                ;;
            --version=*) version=${1#*=} ;;
            --bin-dir)
                [ $# -ge 2 ] || die "--bin-dir needs a value"
                bin_dir=$2
                shift
                ;;
            --bin-dir=*) bin_dir=${1#*=} ;;
            --source) source=yes ;;
            --systemd) systemd=yes ;;
            --no-config) write_config=no ;;
            -h | --help)
                usage
                exit 0
                ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
        shift
    done
    version=${version#v}

    # The relay is a server: as root it goes where system services go.
    system=no
    if [ "$component" = relay ] && [ "$(id -u)" = 0 ]; then
        system=yes
    fi
    if [ "$component" = relay ] && [ "$systemd" = yes ] && [ "$system" = no ]; then
        die "the relay's systemd service is a system service; run this as root, for example with sudo"
    fi
    if [ -z "$bin_dir" ]; then
        if [ "$system" = yes ]; then
            bin_dir=$SYSROOT/usr/local/bin
        else
            bin_dir=$HOME/.local/bin
        fi
    fi
    case $bin_dir in
        /*) ;;
        *) bin_dir=$(pwd)/$bin_dir ;;
    esac

    if [ "$source" = no ]; then
        os=$(uname -s)
        arch=$(uname -m)
        if [ "$os" != Linux ] || { [ "$arch" != x86_64 ] && [ "$arch" != amd64 ]; }; then
            die "prebuilt binaries are for x86-64 Linux, not $os $arch; use --source to build $name with Cargo"
        fi
        need tar
    fi

    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    if [ "$source" = yes ]; then
        build
    else
        download
    fi

    target=$bin_dir/$name
    old=
    if [ -x "$target" ]; then
        old=$("$target" --version 2>/dev/null || true)
    fi
    mkdir -p "$bin_dir"
    # A new file moved over the old one, so that what runs the old binary
    # keeps running and nothing sees a half-written file.
    cp "$work/bin" "$bin_dir/.$name.new.$$"
    chmod 755 "$bin_dir/.$name.new.$$"
    mv -f "$bin_dir/.$name.new.$$" "$target"
    new=$("$target" --version) || die "the installed $target does not run"
    if [ -n "$old" ] && [ "$old" != "$new" ]; then
        say "Replaced $name ${old#"$name" } at $target with ${new#"$name" }"
    else
        say "Installed $new at $target"
    fi

    if [ "$system" = yes ]; then
        config=$SYSROOT/etc/loom/relay.yaml
    else
        config=$("$target" config path)
    fi
    created=no
    if [ -e "$config" ]; then
        "$target" --config "$config" config check >/dev/null 2>&1 ||
            say "Warning: $config does not pass '$name config check' with this version."
    elif [ "$write_config" = yes ]; then
        "$target" --config "$config" config init
        created=yes
    fi

    if [ "$component" = agent ]; then
        if [ "$systemd" = yes ]; then
            agent_service
        else
            # Agents report the version they run; count the ones on another.
            running=$("$target" ps --json 2>/dev/null | grep '"version"' |
                grep -cvF "\"${new#loom-agent }\"" || true)
            if [ "${running:-0}" -gt 0 ]; then
                say "$running running agent(s) have another version. Restart them to use this one:"
                say "  $target stop --all"
            fi
        fi
    else
        if [ "$systemd" = yes ]; then
            relay_service
        elif [ "$old" != "$new" ] && command -v systemctl >/dev/null 2>&1 &&
            systemctl is-active --quiet "$unit" 2>/dev/null; then
            say "$unit still runs the old version. Restart it to use this one:"
            say "  systemctl restart $unit"
        fi
        if [ "$created" = yes ]; then
            say "Edit $config before agents use the relay:"
            say "  listen: the address to serve on (127.0.0.1:8080 reaches this host only)"
            say "  allowed_hosts: the ID of each agent's host, from 'loom-agent host'"
            say "  tls or cloudflare_access, unless a proxy in front of it takes care of them"
            if [ "$systemd" = yes ]; then
                say "Then restart it: systemctl restart $unit"
            fi
        fi
    fi

    case :$PATH: in
        *:"$bin_dir":*) ;;
        *)
            say "$bin_dir is not on your PATH. Add it, for example in ~/.profile:"
            say "  export PATH=\"$bin_dir:\$PATH\""
            ;;
    esac
    found=$(command -v "$name" 2>/dev/null || true)
    if [ -n "$found" ] && [ "$found" != "$target" ]; then
        say "Warning: $name on your PATH is $found, which comes before $target."
    fi
    if [ "$component" = agent ]; then
        say "Clients that reach this host over SSH may not get $bin_dir on PATH; set"
        say "remote_bin: $target for this host in their client.yaml."
    fi
}

main "$@"
