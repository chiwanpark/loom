# Releasing

Loom's binaries are published as archives on [GitHub releases](https://github.com/chiwanpark/loom/releases):

| Archive | Contents | Runs on |
| --- | --- | --- |
| `loom-<version>-x86_64-unknown-linux-gnu.tar.gz` | `loom` (GUI) and `loom-agent` | x86-64 Linux with glibc 2.39 or newer (Ubuntu 24.04, Debian 13), `libxkbcommon`, Wayland, and a Vulkan driver |
| `loom-agent-<version>-x86_64-unknown-linux-musl.tar.gz` | `loom-agent` and its systemd unit | any x86-64 Linux; statically linked |
| `loom-relay-<version>-x86_64-unknown-linux-musl.tar.gz` | `loom-relay` and its systemd unit | any x86-64 Linux; statically linked |
| `loom-agent-<version>-aarch64-unknown-linux-musl.tar.gz` | `loom-agent` and its systemd unit | any arm64 Linux; statically linked |
| `loom-relay-<version>-aarch64-unknown-linux-musl.tar.gz` | `loom-relay` and its systemd unit | any arm64 Linux; statically linked |

Each architecture is built natively, the arm64 archives on GitHub's `ubuntu-24.04-arm` runners.

`scripts/install.sh` downloads the agent and relay archives and `SHA256SUMS` from the latest published release, so keep those names when changing the build. Drafts are not visible to it, and until a release is published it can only build from source (`--source`).

Every crate shares the version in `[workspace.package]` of `Cargo.toml`. The binaries print it with `--version`, followed by the commit they were built from: `loom-agent 0.1.0 (b89ff2d9f 2026-11-11)`.

The version comes from the commit messages, which follow [Conventional Commits](https://www.conventionalcommits.org) (`type(scope): summary`). [git-cliff](https://git-cliff.org) reads them with the rules in `cliff.toml`, and `scripts/release-version.sh` applies the result.

The changelog is written by hand: a user-facing change adds an entry for users to the `## Unreleased` list at the top of `CHANGELOG.md`, starting that list if there is none. At release time, `scripts/release-version.sh` renames that heading to the version. If there is no such list, git-cliff writes the section from the summaries of the changelog commits instead.

## Day-to-day flow

1. Merge changes into `main` with conventional commit messages, and with their `## Unreleased` entries when users notice them. Only `feat`, `fix`, and `perf` commits and breaking changes (`feat!:`, or a `BREAKING CHANGE:` footer) call for a release; `docs`, `test`, `refactor`, `build`, `ci`, and `chore` commits ship with the next one.
2. The `Release` workflow turns the commits since the last release tag into a `chore(release): vX.Y.Z` pull request from the `release-next` branch. It bumps the version in `Cargo.toml` and `Cargo.lock` and gives the `## Unreleased` list the version's heading in `CHANGELOG.md`. Later pushes to `main` rebuild the same pull request from `main`, so fix the wording of an entry on `main`; an edit made only in the pull request is lost on the next push.
3. Merge that pull request when you want to ship. The same workflow builds the archives on the merge commit and creates a draft GitHub release with them, a `SHA256SUMS` file, and the changelog section as notes. The run summary shows the commands for the next step.
4. Download and try the archives (`gh release download vX.Y.Z`), then publish the draft from the releases page or with `gh release edit vX.Y.Z --draft=false --latest`. Publishing creates the `vX.Y.Z` tag on the merge commit.

No new version pull request is opened while a version is waiting for its tag, because the next version is computed from the tag. To drop a drafted version, delete the draft (`gh release delete vX.Y.Z`) and revert the version commit; deleting only the draft builds it again on the next push to `main`.

## Bump policy

While Loom is on `0.x`, `cliff.toml` bumps:

- the patch version for `fix` and `perf` commits;
- the minor version for `feat` commits **and** breaking changes, because `0.x` minors may break.

Leaving `0.x` is a deliberate decision: set the version to `1.0.0` by hand in a release pull request, and change `[bump]` in `cliff.toml` so breaking changes bump the major version.

## Repository settings

- `Settings` → `Actions` → `General`: allow GitHub Actions to create and approve pull requests, so the version pull request can be opened.
- Protect `main` and require the `CI` checks (`Format and lint`, `Test`, `Check licenses and sources`, `Scripts`, and the three `Release build` jobs).
- Pull requests opened with the workflow's `GITHUB_TOKEN` do not trigger other workflows, so CI does not run on the version pull request by itself. Close and reopen it, or push an empty commit to `release-next`, to run the checks before merging. Its changes are only the version and changelog.

## Manual fallback

If Actions is unavailable, with [git-cliff](https://git-cliff.org/docs/installation) installed:

```sh
scripts/release-version.sh          # prints the new version
git commit -am "chore(release): vX.Y.Z"
git push

sudo apt install musl-tools
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl -p loom-agent -p loom-relay
cargo build --release --locked --target x86_64-unknown-linux-gnu -p loom-gui -p loom-agent
scripts/package-release.sh x86_64-unknown-linux-musl loom-agent loom-agent
scripts/package-release.sh x86_64-unknown-linux-musl loom-relay loom-relay
scripts/package-release.sh x86_64-unknown-linux-gnu loom loom loom-agent

# On an arm64 machine, for the arm64 archives (the cc crate needs to be told
# about musl-gcc there). Copy them into dist/ here before the checksums.
rustup target add aarch64-unknown-linux-musl
CC_aarch64_unknown_linux_musl=musl-gcc cargo build --release --locked --target aarch64-unknown-linux-musl -p loom-agent -p loom-relay
scripts/package-release.sh aarch64-unknown-linux-musl loom-agent loom-agent
scripts/package-release.sh aarch64-unknown-linux-musl loom-relay loom-relay

(cd dist && sha256sum -- *.tar.gz > SHA256SUMS)
gh release create vX.Y.Z --target main --title vX.Y.Z --notes-file <(sed -n '/^## X.Y.Z$/,/^## /{//!p}' CHANGELOG.md) dist/*
```

Build the GUI on a distribution no newer than the oldest one it should run on, since it needs that glibc or newer.
