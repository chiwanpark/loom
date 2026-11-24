#!/bin/sh
# Installs Loom's agent or desktop client.
#
# By default it downloads the binaries from the latest GitHub release, checks
# them against the release's SHA256SUMS, installs them, and writes a
# configuration if there is none. Running it again upgrades them.
#
#   curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s agent
#   curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s desktop
#
# Run with --help for the options.
set -eu

REPO_URL=https://github.com/chiwanpark/loom
# Where the releases and the repository's files are. Tests point these at
# local copies.
RELEASES_URL=${LOOM_RELEASES_URL:-$REPO_URL/releases}
RAW_URL=${LOOM_RAW_URL:-https://raw.githubusercontent.com/chiwanpark/loom}

usage() {
    cat <<EOF
Installs Loom's agent or desktop client.

Usage: install.sh agent|desktop [options]

  agent    loom-agent, for the current user: in ~/.local/bin, with its
           configuration in ~/.config/loom/agent.yaml.
  desktop  The desktop client loom and the loom-agent it starts, for the
           current user, like the agent. It also installs the desktop entry
           and icons in ~/.local/share, so that the desktop's menus list
           Loom.

Options:
  --version <version>  Install this release, such as 0.2.0, instead of the latest.
  --bin-dir <dir>      Install into this directory instead.
  --source             Build it with Cargo instead of downloading it, for hosts
                       without a prebuilt binary.
  --no-config          Don't write a configuration when there is none.
  -h, --help           Show this help.

Prebuilt binaries are for x86-64 and arm64 Linux; the desktop client's are
for x86-64 Linux only. Desktop clients that are already running keep the
old version until they are restarted.
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

# Downloads the release archive and leaves its binaries in $work/bin, and
# its desktop entry and icons in $work/share.
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
    say "Downloading $name $version for $TARGET"
    fetch "$dir/$archive" "$work/$archive" || die "could not download $dir/$archive"
    actual=$(sha256 "$work/$archive")
    [ "$actual" = "$expected" ] ||
        die "$archive does not match its checksum in SHA256SUMS (expected $expected, got $actual)"
    mkdir "$work/unpack"
    tar -xzf "$work/$archive" -C "$work/unpack"
    stem=$work/unpack/${archive%.tar.gz}
    mkdir "$work/bin"
    for binary in $binaries; do
        [ -f "$stem/$binary" ] || die "$archive has no $binary"
        cp "$stem/$binary" "$work/bin/"
    done
    if [ "$component" = desktop ] && [ -d "$stem/share" ]; then
        cp -R "$stem/share" "$work/share"
    fi
}

# Builds the binaries with Cargo into $work/bin, and fetches the desktop
# entry and icons for the desktop client.
build() {
    need cargo
    # Cargo warns when the directory it installs into is not on PATH; this
    # one is only a step on the way to $bin_dir.
    path=$work/root/bin:$PATH
    if [ -n "$version" ]; then
        ref=v$version
    else
        ref=main
    fi
    if [ "$component" = desktop ]; then
        fetch_share "$RAW_URL/$ref"
    fi
    mkdir "$work/bin"
    for binary in $binaries; do
        package=$binary
        [ "$binary" != loom ] || package=loom-gui
        if [ -n "$version" ]; then
            say "Building $binary $version with Cargo"
            PATH=$path cargo install --locked --git "$REPO_URL" --tag "$ref" --root "$work/root" "$package"
        else
            say "Building $binary from the main branch with Cargo"
            PATH=$path cargo install --locked --git "$REPO_URL" --branch main --root "$work/root" "$package"
        fi
        cp "$work/root/bin/$binary" "$work/bin/"
    done
}

# Fetches the desktop entry and the icons from the repository at $1 into
# $work/share, laid out as in the release archive.
fetch_share() {
    share=$work/share
    mkdir -p "$share/applications" "$share/icons/hicolor/scalable/apps"
    fetch "$1/contrib/desktop/$desktop_file" "$share/applications/$desktop_file" ||
        die "could not download the desktop entry from $1"
    for size in 64 128 256 512; do
        mkdir -p "$share/icons/hicolor/${size}x$size/apps"
        fetch "$1/assets/logo-$size.png" "$share/icons/hicolor/${size}x$size/apps/$icon.png" ||
            die "could not download the icons from $1"
    done
    fetch "$1/assets/logo.svg" "$share/icons/hicolor/scalable/apps/$icon.svg" ||
        die "could not download the icons from $1"
}

# Installs the binary $1 from $work/bin into $bin_dir.
install_binary() {
    dest=$bin_dir/$1
    old=
    if [ -x "$dest" ]; then
        old=$("$dest" --version 2>/dev/null || true)
    fi
    # A new file moved over the old one, so that what runs the old binary
    # keeps running and nothing sees a half-written file.
    cp "$work/bin/$1" "$bin_dir/.$1.new.$$"
    chmod 755 "$bin_dir/.$1.new.$$"
    mv -f "$bin_dir/.$1.new.$$" "$dest"
    new=$("$dest" --version) || die "the installed $dest does not run"
    if [ -n "$old" ] && [ "$old" != "$new" ]; then
        say "Replaced $1 ${old#"$1" } at $dest with ${new#"$1" }"
    else
        say "Installed $new at $dest"
    fi
}

# The Exec value of the desktop entry that starts $1. Arguments with spaces
# are quoted; --bin-dir is checked for other special characters before this.
desktop_exec() {
    case $1 in
        *" "*) printf '"%s"\n' "$1" ;;
        *) printf '%s\n' "$1" ;;
    esac
}

# Installs the desktop entry, pointed at the installed loom, and the icons
# from $work/share into the user's data directory.
desktop_entry() {
    if [ ! -f "$work/share/applications/$desktop_file" ]; then
        say "The release has no desktop entry; install a newer release to get one."
        return
    fi
    data=${XDG_DATA_HOME:-$HOME/.local/share}
    apps=$data/applications
    mkdir -p "$apps"
    EXEC=$(desktop_exec "$bin_dir/loom") awk '
        /^Exec=/ { print "Exec=" ENVIRON["EXEC"]; next }
        { print }
    ' "$work/share/applications/$desktop_file" >"$apps/.$desktop_file.new.$$"
    mv -f "$apps/.$desktop_file.new.$$" "$apps/$desktop_file"
    say "Installed the desktop entry $apps/$desktop_file"
    (cd "$work/share" && find icons -type f) | while IFS= read -r file; do
        mkdir -p "$data/${file%/*}"
        cp "$work/share/$file" "$data/$file"
    done
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$apps" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$data/icons/hicolor/index.theme" ]; then
        gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
    fi
}

main() {
    case ${1:-} in
        agent | desktop) component=$1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        "") die "say what to install: install.sh agent|desktop [options] (see --help)" ;;
        *) die "unknown component: $1; install.sh installs agent or desktop (see --help)" ;;
    esac
    shift
    # $name is the release archive's name, $binaries what it installs, and
    # $server the binary whose configuration it writes.
    if [ "$component" = desktop ]; then
        name=loom
        binaries="loom loom-agent"
    else
        name=loom-agent
        binaries=loom-agent
    fi
    server=loom-agent
    desktop_file=dev.loom.Loom.desktop
    icon=dev.loom.Loom
    version=
    bin_dir=
    source=no
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

    if [ -z "$bin_dir" ]; then
        bin_dir=$HOME/.local/bin
    fi
    case $bin_dir in
        /*) ;;
        *) bin_dir=$(pwd)/$bin_dir ;;
    esac
    if [ "$component" = desktop ]; then
        case $bin_dir in
            *[!A-Za-z0-9/._+\ -]*) die "the desktop entry needs --bin-dir without special characters: $bin_dir" ;;
        esac
    fi

    if [ "$source" = no ]; then
        os=$(uname -s)
        arch=$(uname -m)
        case $component:$os/$arch in
            desktop:Linux/x86_64 | desktop:Linux/amd64) TARGET=x86_64-unknown-linux-gnu ;;
            desktop:*) die "the prebuilt desktop client is for x86-64 Linux, not $os $arch; use --source to build it with Cargo" ;;
            *:Linux/x86_64 | *:Linux/amd64) TARGET=x86_64-unknown-linux-musl ;;
            *:Linux/aarch64 | *:Linux/arm64) TARGET=aarch64-unknown-linux-musl ;;
            *) die "prebuilt binaries are for x86-64 and arm64 Linux, not $os $arch; use --source to build $name with Cargo" ;;
        esac
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

    mkdir -p "$bin_dir"
    for binary in $binaries; do
        [ "$binary" = "$server" ] || install_binary "$binary"
    done
    install_binary "$server"
    target=$bin_dir/$server
    if [ "$component" = desktop ]; then
        desktop_entry
    fi

    config=$("$target" config path)
    if [ -e "$config" ]; then
        "$target" --config "$config" config check >/dev/null 2>&1 ||
            say "Warning: $config does not pass '$server config check' with this version."
    elif [ "$write_config" = yes ]; then
        "$target" --config "$config" config init
    fi

    case :$PATH: in
        *:"$bin_dir":*) ;;
        *)
            say "$bin_dir is not on your PATH. Add it, for example in ~/.profile:"
            say "  export PATH=\"$bin_dir:\$PATH\""
            ;;
    esac
    for binary in $binaries; do
        found=$(command -v "$binary" 2>/dev/null || true)
        if [ -n "$found" ] && [ "$found" != "$bin_dir/$binary" ]; then
            say "Warning: $binary on your PATH is $found, which comes before $bin_dir/$binary."
        fi
    done
}

main "$@"
