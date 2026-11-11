#!/usr/bin/env bash
# Prepares the next release from the commits since the last release tag: picks
# the version (cliff.toml has the rules), sets it in Cargo.toml and Cargo.lock,
# and adds its section to CHANGELOG.md.
#
# Prints the new version, or nothing when there is nothing to release: the
# current version is not tagged yet (it is still waiting to be released), or
# no commit since the tag is a feature, fix, performance, or breaking change.
#
# Needs git-cliff (https://git-cliff.org) and the release tags, so run it in a
# full clone rather than a shallow one.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

current=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
if [ -z "$current" ]; then
    echo "no version in [workspace.package] of Cargo.toml" >&2
    exit 1
fi
if ! git rev-parse -q --verify "refs/tags/v$current" >/dev/null; then
    echo "v$current has no tag yet, so it is still waiting to be released." >&2
    exit 0
fi

section=$(git-cliff --unreleased --bump --strip all 2>/dev/null)
if ! grep -q '^- ' <<<"$section"; then
    echo "Nothing to release since v$current." >&2
    exit 0
fi
next=$(git-cliff --bumped-version 2>/dev/null)
next=${next#v}

sed -i "/^\[workspace\.package\]/,/^\[/s/^version = \".*\"$/version = \"$next\"/" Cargo.toml
cargo update --workspace --quiet

# The new section goes above the newest one, after the file's introduction.
{
    sed '/^## /,$d' CHANGELOG.md
    printf '%s\n\n' "$section"
    sed -n '/^## /,$p' CHANGELOG.md
} >CHANGELOG.md.new
mv CHANGELOG.md.new CHANGELOG.md

echo "$next"
