# Releasing

Loom's binaries are published as archives on [GitHub releases](https://github.com/chiwanpark/loom/releases):

| Archive | Contents | Runs on |
| --- | --- | --- |
| `loom-<version>-x86_64-unknown-linux-gnu.tar.gz` | `loom` (GUI) and `loom-agent` | x86-64 Linux with glibc 2.39 or newer (Ubuntu 24.04, Debian 13), `libxkbcommon`, Wayland, and a Vulkan driver |
| `loom-agent-<version>-x86_64-unknown-linux-musl.tar.gz` | `loom-agent` and its systemd unit | any x86-64 Linux; statically linked |
| `loom-relay-<version>-x86_64-unknown-linux-musl.tar.gz` | `loom-relay` | any x86-64 Linux; statically linked |

Every crate shares the version in `[workspace.package]` of `Cargo.toml`. The binaries print it with `--version`, followed by the commit they were built from: `loom-agent 0.1.0 (b89ff2d9f 2026-11-11)`.

The version and changelog come from the commit messages, which follow [Conventional Commits](https://www.conventionalcommits.org) (`type(scope): summary`). [git-cliff](https://git-cliff.org) reads them with the rules in `cliff.toml`, and `scripts/release-version.sh` applies the result.

## Day-to-day flow

1. Merge changes into `main` with conventional commit messages. Only `feat`, `fix`, and `perf` commits and breaking changes (`feat!:`, or a `BREAKING CHANGE:` footer) reach the changelog; `docs`, `test`, `refactor`, `build`, `ci`, and `chore` commits ship without an entry.
2. The `Release` workflow collects the commits since the last release tag into a `chore(release): vX.Y.Z` pull request from the `release-next` branch. It bumps the version in `Cargo.toml` and `Cargo.lock` and adds the version's section to `CHANGELOG.md`. Later pushes to `main` update the same pull request. Edit the changelog in the pull request if an entry needs better wording.
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
- Protect `main` and require the `CI` checks (`Format and lint`, `Test`, `Check licenses and sources`, and the two `Release build` jobs).
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
(cd dist && sha256sum -- *.tar.gz > SHA256SUMS)
gh release create vX.Y.Z --target main --title vX.Y.Z --notes-file <(sed -n '/^## X.Y.Z$/,/^## /{//!p}' CHANGELOG.md) dist/*
```

Build the GUI on a distribution no newer than the oldest one it should run on, since it needs that glibc or newer.
