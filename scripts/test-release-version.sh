#!/usr/bin/env bash
# Tests scripts/release-version.sh in throwaway git repositories, with a stub
# git-cliff that reports a given version and section and a stub cargo. It
# needs no network and leaves this repository alone.
#
# Usage: scripts/test-release-version.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

script=$PWD/scripts/release-version.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
failures=0

mkdir "$root/bin"
# git-cliff prints $STUB_SECTION for the unreleased section and $STUB_NEXT
# for the bumped version.
cat >"$root/bin/git-cliff" <<'EOF'
#!/bin/sh
case "$*" in
    "--unreleased --bump --strip all") printf '%s\n' "$STUB_SECTION" ;;
    "--bumped-version") printf 'v%s\n' "$STUB_NEXT" ;;
    *) echo "unexpected git-cliff $*" >&2; exit 2 ;;
esac
EOF
# cargo records what it was asked to do.
cat >"$root/bin/cargo" <<'EOF'
#!/bin/sh
echo "cargo $*" >>"$STUB_LOG"
EOF
chmod +x "$root/bin/git-cliff" "$root/bin/cargo"

generated='## 0.2.0

### Added

- **gui:** Show task numbers ([abc1234](https://example.com))'

# A repository on version 0.1.0 with the given CHANGELOG.md, tagged v0.1.0
# unless $2 is "untagged". Prints its path.
repo() {
    local dir
    dir=$(mktemp -d "$root/repo.XXXX")
    git -C "$dir" init -q
    mkdir "$dir/scripts"
    cp "$script" "$dir/scripts/"
    printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.1.0"\nedition = "2024"\n' >"$dir/Cargo.toml"
    printf '%s\n' "$1" >"$dir/CHANGELOG.md"
    git -C "$dir" add -A
    git -C "$dir" -c user.name=test -c user.email=test@example.com commit -qm "chore: start"
    [ "${2:-}" = untagged ] || git -C "$dir" tag v0.1.0
    printf '%s\n' "$dir"
}

# Runs the script in $1 with the stub section $2; sets $out and $log. A
# failing script counts as a failure, and its error is shown.
run() {
    : >"$root/cargo.log"
    if ! out=$(cd "$1" && PATH="$root/bin:$PATH" STUB_SECTION=$2 STUB_NEXT=0.2.0 \
        STUB_LOG="$root/cargo.log" scripts/release-version.sh 2>"$root/stderr"); then
        echo "FAIL: release-version.sh exited with an error:"
        cat "$root/stderr"
        failures=$((failures + 1))
    fi
    log=$(cat "$root/cargo.log")
}

check() {
    local name=$1 expected=$2 actual=$3
    if [ "$expected" != "$actual" ]; then
        echo "FAIL: $name"
        diff -u <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") || true
        failures=$((failures + 1))
    else
        echo "ok: $name"
    fi
}

intro='# Changelog

All notable changes are listed here.'

# The hand-written list becomes the version's section.
dir=$(repo "$intro

## Unreleased

- Notes and tasks for each session.
- The protocol version is now 9.

## 0.1.0

The first release.")
run "$dir" "$generated"
check "the new version is printed" 0.2.0 "$out"
check "the hand-written list is the new section" "$intro

## 0.2.0

- Notes and tasks for each session.
- The protocol version is now 9.

## 0.1.0

The first release." "$(cat "$dir/CHANGELOG.md")"
check "Cargo.toml has the new version" 'version = "0.2.0"' "$(grep '^version' "$dir/Cargo.toml")"
check "Cargo.lock is updated" "cargo update --workspace --quiet" "$log"

# Without a hand-written list, git-cliff writes the section.
dir=$(repo "$intro

## 0.1.0

The first release.")
run "$dir" "$generated"
check "git-cliff writes the section without a list" "$intro

$generated

## 0.1.0

The first release." "$(cat "$dir/CHANGELOG.md")"

# An empty "## Unreleased" makes way for git-cliff's section.
dir=$(repo "$intro

## Unreleased

## 0.1.0

The first release.")
run "$dir" "$generated"
check "an empty Unreleased heading is replaced" "$intro

$generated

## 0.1.0

The first release." "$(cat "$dir/CHANGELOG.md")"

# Nothing changes while there is nothing to release.
changelog="$intro

## Unreleased

- A fix.

## 0.1.0

The first release."
dir=$(repo "$changelog")
run "$dir" "## 0.2.0"
check "nothing is released without changelog commits" "" "$out"
check "the changelog stays without changelog commits" "$changelog" "$(cat "$dir/CHANGELOG.md")"
check "the version stays without changelog commits" 'version = "0.1.0"' "$(grep '^version' "$dir/Cargo.toml")"

# Nor while the current version waits for its tag.
dir=$(repo "$changelog" untagged)
run "$dir" "$generated"
check "nothing is released before the tag" "" "$out"
check "the changelog stays before the tag" "$changelog" "$(cat "$dir/CHANGELOG.md")"
check "cargo is not run before the tag" "" "$log"

if [ "$failures" -gt 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "all checks passed"
