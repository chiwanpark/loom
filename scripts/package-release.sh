#!/usr/bin/env bash
# Packs release binaries built with `cargo build --release --target <target>`
# into dist/<name>-<version>-<target>.tar.gz, together with the README,
# license, and notice (and the systemd unit when loom-agent is included).
#
# Usage: scripts/package-release.sh <target> <name> <binary>...
# Example: scripts/package-release.sh x86_64-unknown-linux-musl loom-agent loom-agent
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [ $# -lt 3 ]; then
    echo "usage: $0 <target> <name> <binary>..." >&2
    exit 2
fi
target=$1
name=$2
shift 2

version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
stem="$name-$version-$target"
target_dir=${CARGO_TARGET_DIR:-target}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$stem"

for binary in "$@"; do
    install -m 755 "$target_dir/$target/release/$binary" "$stage/$stem/"
    if [ "$binary" = loom-agent ]; then
        install -m 644 contrib/systemd/loom-agent.service "$stage/$stem/"
    fi
done
install -m 644 README.md LICENSE NOTICE CHANGELOG.md "$stage/$stem/"

mkdir -p dist
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "dist/$stem.tar.gz" "$stem"
echo "dist/$stem.tar.gz"
