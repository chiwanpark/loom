#!/usr/bin/env bash
# Tests scripts/install.sh against fake releases on disk, with stub
# binaries, a stub systemctl, and a stub cargo, in throwaway home and system
# directories. It needs no network and starts nothing.
#
# Usage: scripts/test-install.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

installer=$PWD/scripts/install.sh
target=x86_64-unknown-linux-musl
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
releases=$root/releases
sysroot=$root/sys
failures=0

# A loom-agent or loom-relay ($1) of version $2 that answers the commands
# the installer runs. ps lists one agent on the version in $STUB_RUNNING, if
# it is set.
stub() {
    local name=$1 version=$2 file=agent.yaml
    [ "$name" = loom-agent ] || file=relay.yaml
    cat <<EOF
#!/bin/sh
config=\${LOOM_CONFIG:-\$HOME/.config/loom/$file}
if [ "\${1:-}" = --config ]; then
    config=\$2
    shift 2
fi
case "\$1 \${2:-}" in
    "--version ") echo "$name $version (stub)" ;;
    "config path") echo "\$config" ;;
    "config init") mkdir -p "\$(dirname "\$config")"; echo "version: 1" >"\$config"; echo "wrote \$config" ;;
    "config check") ! grep -q broken "\$config" ;;
    "ps --json")
        if [ -n "\${STUB_RUNNING:-}" ]; then
            printf '[\n  {\n    "pid": 1,\n    "version": "%s"\n  }\n]\n' "\$STUB_RUNNING"
        else
            echo "[]"
        fi ;;
    *) echo "stub $name: unexpected \$*" >&2; exit 2 ;;
esac
EOF
}

# Packs the archive of $1 for release v$2 into $3, as the Release workflow
# lays it out, with a stub of version $4.
pack() {
    local name=$1 version=$2 dir=$3 binary=${4:-$2}
    local stem=$name-$version-$target stage
    stage=$(mktemp -d -p "$root" stage.XXXX)
    mkdir -p "$stage/$stem" "$dir"
    stub "$name" "$binary" >"$stage/$stem/$name"
    chmod 755 "$stage/$stem/$name"
    cp "contrib/systemd/$name.service" "$stage/$stem/"
    tar -C "$stage" -czf "$dir/$stem.tar.gz" "$stem"
}

# Builds release v<version> and makes it the latest when asked.
release() {
    local version=$1 latest=${2:-}
    local dir=$releases/download/v$version
    pack loom-agent "$version" "$dir"
    pack loom-relay "$version" "$dir"
    # The GUI archive is listed first, as sha256sum sorts it so.
    echo "not an archive" >"$dir/loom-$version-x86_64-unknown-linux-gnu.tar.gz"
    (cd "$dir" && sha256sum -- *.tar.gz >SHA256SUMS)
    if [ -n "$latest" ]; then
        rm -rf "$releases/latest"
        mkdir -p "$releases/latest"
        cp -r "$dir" "$releases/latest/download"
    fi
}

# Runs the installer with dash, as `curl ... | sh` does on Debian, in a
# fresh home directory unless $home is set, as root if $STUB_ROOT is set.
# Output goes to $out.
run() {
    home=${home:-$(mktemp -d -p "$root" home.XXXX)}
    out=$root/out
    env -i HOME="$home" PATH="$root/fake-bin:/usr/bin:/bin" \
        LOOM_RELEASES_URL="file://$releases" LOOM_RAW_URL="file://$root/raw" \
        LOOM_INSTALL_SYSROOT="$sysroot" \
        STUB_RUNNING="${STUB_RUNNING:-}" STUB_ROOT="${STUB_ROOT:-}" \
        dash "$installer" "$@" >"$out" 2>&1
}

check() {
    local name=$1
    shift
    if "$@"; then
        echo "ok: $name"
    else
        echo "FAILED: $name"
        sed 's/^/    /' "$out"
        failures=$((failures + 1))
    fi
}

has() {
    grep -qF -- "$1" "$out"
}

lacks() {
    ! has "$1"
}

called() {
    grep -sqx -- "$1" "$root/systemctl.log"
}

not_called() {
    ! grep -sq -- "$1" "$root/systemctl.log"
}

agent_version() {
    "$home/.local/bin/loom-agent" --version
}

mkdir -p "$root/fake-bin"
# id says root when $STUB_ROOT is set.
cat >"$root/fake-bin/id" <<'EOF'
#!/bin/sh
if [ -n "$STUB_ROOT" ] && [ "$*" = -u ]; then echo 0; else exec /usr/bin/id "$@"; fi
EOF
chmod 755 "$root/fake-bin/id"
release 0.1.0
release 0.2.0 latest

# What to install.
unset home
if run; then status=0; else status=$?; fi
check "needs a component" [ $status -ne 0 ]
check "says to name a component" has "say what to install: install.sh agent|relay"
if run client; then status=0; else status=$?; fi
check "rejects an unknown component" [ $status -ne 0 ]
check "names the unknown component" has "unknown component: client"
run --help || true
check "shows help without a component" has "Usage: install.sh agent|relay"
check "installs nothing without a component" [ ! -e "$home/.local" ]

# The latest release, into a new home.
unset home
if run agent; then status=0; else status=$?; fi
check "installs the latest release" [ $status -eq 0 ]
check "the latest is 0.2.0" [ "$(agent_version)" = "loom-agent 0.2.0 (stub)" ]
check "writes a configuration" [ -f "$home/.config/loom/agent.yaml" ]
check "says the bin directory is not on PATH" has "is not on your PATH"
check "names the binary for SSH clients" has "remote_bin: $home/.local/bin/loom-agent"
check "leaves no temporary file" [ "$(ls -A "$home/.local/bin")" = loom-agent ]

# Another version over it keeps the configuration.
echo "version: 1 # mine" >"$home/.config/loom/agent.yaml"
if run agent --version v0.1.0; then status=0; else status=$?; fi
check "installs a given version" [ $status -eq 0 ]
check "replaces the binary" [ "$(agent_version)" = "loom-agent 0.1.0 (stub)" ]
check "says which version it replaced" has "Replaced loom-agent 0.2.0 (stub)"
check "keeps the configuration" grep -q mine "$home/.config/loom/agent.yaml"
check "does not warn about a configuration that passes" lacks "Warning:"
echo "broken: yes" >>"$home/.config/loom/agent.yaml"
run agent || true
check "warns about a configuration that fails its check" \
    has "Warning: $home/.config/loom/agent.yaml does not pass 'loom-agent config check'"
echo "version: 1" >"$home/.config/loom/agent.yaml"

# Agents still on another version are pointed out.
if STUB_RUNNING="0.1.0 (stub)" run agent; then status=0; else status=$?; fi
check "upgrades with an agent running" [ $status -eq 0 ]
check "asks to restart agents on another version" has "1 running agent(s) have another version"
STUB_RUNNING="0.2.0 (stub)" run agent || true
check "does not ask when agents run this version" lacks "running agent(s)"

# A missing release fails and keeps the installed binary.
if run agent --version 9.9.9; then status=0; else status=$?; fi
check "fails for a missing release" [ $status -ne 0 ]
check "names the missing release" has "is there a published release v9.9.9?"
check "keeps the binary after a failure" [ "$(agent_version)" = "loom-agent 0.2.0 (stub)" ]

# A release without the archive fails.
mkdir -p "$releases/download/v0.2.5"
grep -v loom-agent "$releases/download/v0.2.0/SHA256SUMS" >"$releases/download/v0.2.5/SHA256SUMS"
if run agent --version 0.2.5; then status=0; else status=$?; fi
check "fails for a release without loom-agent" [ $status -ne 0 ]
check "says the release has no loom-agent" has "has no loom-agent archive for $target"

# A release whose archive is another version fails.
mkdir -p "$releases/download/v0.2.6"
cp "$releases/download/v0.2.0/"* "$releases/download/v0.2.6/"
if run agent --version 0.2.6; then status=0; else status=$?; fi
check "fails for a release of another version" [ $status -ne 0 ]
check "says the release lists another version" has "release v0.2.6 lists loom-agent 0.2.0"

# An archive that does not match its checksum is not installed: a
# well-formed archive with another binary in it, so only the checksum stops
# it.
release 0.3.0 latest
pack loom-agent 0.3.0 "$releases/latest/download" tampered
if run agent; then status=0; else status=$?; fi
check "rejects an archive that fails its checksum" [ $status -ne 0 ]
check "says the checksum does not match" has "does not match its checksum"
check "keeps the binary after a bad download" [ "$(agent_version)" = "loom-agent 0.2.0 (stub)" ]
release 0.3.0 latest

# Options.
unset home
run agent --no-config --bin-dir "$root/bin dir" || true
check "installs into --bin-dir" [ -x "$root/bin dir/loom-agent" ]
check "--no-config writes no configuration" [ ! -e "$home/.config/loom/agent.yaml" ]
if run agent --frobnicate; then status=0; else status=$?; fi
check "rejects an unknown option" [ $status -ne 0 ]
check "names the unknown option" has "unknown option: --frobnicate"

# The systemd service, with a systemctl that records its calls and says a
# unit is active when $root/active.<unit> exists.
cat >"$root/fake-bin/systemctl" <<EOF
#!/bin/sh
echo "\$*" >>"$root/systemctl.log"
for arg; do [ -n "\$arg" ] || { echo "systemctl: empty argument" >&2; exit 64; }; done
case "\$*" in
    *is-active*) for unit; do :; done; [ -f "$root/active.\$unit" ] ;;
esac
EOF
chmod 755 "$root/fake-bin/systemctl"
unset home
run agent --systemd || true
unit=$home/.config/systemd/user/loom-agent.service
check "installs the unit" [ -f "$unit" ]
check "points the unit at the installed binary" \
    grep -sqx "ExecStart=$home/.local/bin/loom-agent serve --default" "$unit"
check "does not start the service without a default workspace" not_called enable
check "says to set the default workspace" has "lifecycle.default_workspace"

rm -f "$root/systemctl.log"
printf 'version: 1\nlifecycle:\n  default_workspace: ~/src\n' >"$home/.config/loom/agent.yaml"
run agent --systemd || true
check "starts the service" called "--user enable --now loom-agent.service"
touch "$root/active.loom-agent.service"
run agent --systemd || true
check "restarts a running service" called "--user restart loom-agent.service"

# The relay, for the current user.
rm -f "$root/systemctl.log"
unset home
if run relay; then status=0; else status=$?; fi
check "installs the relay" [ $status -eq 0 ]
check "installs the relay into ~/.local/bin" \
    [ "$("$home/.local/bin/loom-relay" --version)" = "loom-relay 0.3.0 (stub)" ]
check "writes the relay's configuration" [ -f "$home/.config/loom/relay.yaml" ]
check "says what to set in the new configuration" has "allowed_hosts: the ID of each agent's host"
check "does not mention remote_bin for the relay" lacks "remote_bin"
check "installs only the relay" [ "$(ls -A "$home/.local/bin")" = loom-relay ]
run relay || true
check "says nothing about a configuration it did not write" lacks "allowed_hosts"
if run relay --systemd; then status=0; else status=$?; fi
check "needs root for the relay's service" [ $status -ne 0 ]
check "says the relay's service needs root" has "run this as root"

# The relay as root, under $sysroot.
rm -f "$root/systemctl.log"
unset home
export STUB_ROOT=1
if run relay --systemd --version 0.2.0; then status=0; else status=$?; fi
relay=$sysroot/usr/local/bin/loom-relay
config=$sysroot/etc/loom/relay.yaml
unit=$sysroot/etc/systemd/system/loom-relay.service
check "installs the relay for the system" [ $status -eq 0 ]
check "installs the relay into /usr/local/bin" [ "$("$relay" --version)" = "loom-relay 0.2.0 (stub)" ]
check "writes /etc/loom/relay.yaml" [ -f "$config" ]
check "writes nothing into the home directory" [ -z "$(ls -A "$home")" ]
check "installs the system unit" [ -f "$unit" ]
check "points the system unit at the binary and configuration" \
    grep -sqx "ExecStart=$relay --config $config serve" "$unit"
check "keeps the rest of the unit" grep -sqx "DynamicUser=yes" "$unit"
check "reloads systemd" called "daemon-reload"
check "starts the relay's service" called "enable --now loom-relay.service"
check "says to restart it after editing the configuration" has "Then restart it: systemctl restart loom-relay.service"

touch "$root/active.loom-relay.service"
echo "version: 1 # mine" >"$config"
run relay --systemd || true
check "upgrades the relay" has "Replaced loom-relay 0.2.0 (stub) at $relay with 0.3.0 (stub)"
check "restarts the relay's service" called "restart loom-relay.service"
check "keeps the relay's configuration" grep -q mine "$config"

run relay --version 0.2.0 || true
check "says the running relay needs a restart" has "loom-relay.service still runs the old version"
run relay --version 0.2.0 || true
check "does not ask for a restart when nothing changed" lacks "still runs the old version"
unset STUB_ROOT

# Building with Cargo, with a cargo that records its arguments and installs
# a stub, and the unit from the repository at the tag.
# shellcheck disable=SC2016 # the stub expands these itself
{
    echo '#!/bin/sh'
    echo "echo \"\$*\" >\"$root/cargo.log\""
    echo 'while [ $# -gt 0 ]; do [ "$1" != --root ] || dir=$2; name=$1; shift; done'
    echo 'mkdir -p "$dir/bin"'
    echo "cp \"$root/built-\$name\" \"\$dir/bin/\$name\""
} >"$root/fake-bin/cargo"
chmod 755 "$root/fake-bin/cargo"
for name in loom-agent loom-relay; do
    stub "$name" built >"$root/built-$name"
    chmod 755 "$root/built-$name"
done
mkdir -p "$root/raw/v0.2.0/contrib/systemd"
cp contrib/systemd/loom-agent.service "$root/raw/v0.2.0/contrib/systemd/"
unset home
run agent --source --version 0.2.0 --systemd || true
check "builds the tag with Cargo" grep -sqx -- \
    "install --locked --git https://github.com/chiwanpark/loom --tag v0.2.0 --root .*/root loom-agent" \
    "$root/cargo.log"
check "installs what Cargo built" [ "$(agent_version)" = "loom-agent built (stub)" ]
check "fetches the unit from the tag" [ -f "$home/.config/systemd/user/loom-agent.service" ]
check "does not take the build for a loom-agent on PATH" lacks "Warning: loom-agent on your PATH"
unset home
run agent --source || true
check "builds the main branch by default" grep -sq -- "--branch main" "$root/cargo.log"
check "needs no unit without --systemd" [ ! -e "$home/.config/systemd" ]
unset home
run relay --source || true
check "builds the relay with Cargo" grep -sq -- "--branch main --root .*/root loom-relay\$" "$root/cargo.log"
check "installs the relay Cargo built" \
    [ "$("$home/.local/bin/loom-relay" --version)" = "loom-relay built (stub)" ]

if [ $failures -gt 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "all checks passed"
