#!/usr/bin/env bash
# Prepares the next release from the commits since the last release tag: picks
# the version (cliff.toml has the rules), sets it in Cargo.toml and Cargo.lock,
# and gives it a section in CHANGELOG.md.
#
# That section is the hand-written list under "## Unreleased", which user-facing
# changes add to as they land; its heading becomes the version. Without such a
# list, git-cliff writes the section from the commit summaries.
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

# What is under "## Unreleased", up to the next section.
unreleased=$(awk '
    $0 == "## Unreleased" { found = 1; next }
    found && /^## / { exit }
    found { print }
' CHANGELOG.md)

if grep -q '[^[:space:]]' <<<"$unreleased"; then
    sed -i "s/^## Unreleased\$/## $next/" CHANGELOG.md
else
    # An empty "## Unreleased" goes; the new section goes above the newest
    # one, after the file's introduction.
    awk '
        $0 == "## Unreleased" { skip = 1; next }
        skip && /^## / { skip = 0 }
        !skip
    ' CHANGELOG.md >CHANGELOG.md.new
    {
        sed '/^## /,$d' CHANGELOG.md.new
        printf '%s\n\n' "$section"
        sed -n '/^## /,$p' CHANGELOG.md.new
    } >CHANGELOG.md
    rm CHANGELOG.md.new
fi

echo "$next"
