# Installation and updates

## Release archives

The [releases page](https://github.com/chiwanpark/loom/releases) has prebuilt archives for Linux.

| Archive | Contents | Requirements |
| --- | --- | --- |
| `loom-<version>-x86_64-unknown-linux-gnu.tar.gz` | The desktop client `loom` and `loom-agent`, for x86-64. | glibc 2.39 or newer (Ubuntu 24.04, Debian 13), `libxkbcommon`, a Wayland session, and a Vulkan driver. |
| `loom-agent-<version>-<arch>-unknown-linux-musl.tar.gz` | `loom-agent`, statically linked. | Any Linux. |
| `loom-oracle-<version>-<arch>-unknown-linux-musl.tar.gz` | `loom-oracle`, statically linked. | Any Linux. |
| `loom-host-<version>-<arch>-unknown-linux-musl.tar.gz` | `loom-host`, statically linked. | Any Linux. |
| `loom-oracle-<version>-incus-<image arch>.*.tar.xz`, `loom-host-<version>-incus-<image arch>.*.tar.xz` | Incus images of `loom-oracle` and `loom-host`. See [Incus deployment](incus.md). | Incus. |

`<arch>` is `x86_64` for x86-64 and `aarch64` for arm64.
`<image arch>` is `amd64` or `arm64`.

To install an archive manually:

1. Unpack the archive.
2. Put its binaries on your `PATH`, for example in `~/.local/bin`.
3. Compare the archive checksum with `SHA256SUMS` on the release page.

The `share` directory of the desktop archive has the desktop entry and the icons. Its layout is the same as `~/.local/share`.

Each binary shows its version and its commit with `--version`. [RELEASING.md](../RELEASING.md) tells how we make releases.

## Install script

`scripts/install.sh` installs a component from the latest release. It does these steps:

1. It downloads the binary for the architecture of the host.
2. It compares the binary with `SHA256SUMS`.
3. It puts the binary in `~/.local/bin`.
4. It writes a configuration file if there is no configuration file.

To upgrade, run the script again.

### Agent

Install `loom-agent` for the current user:

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s agent
```

The script writes `~/.config/loom/agent.yaml`.

### Desktop client

`scripts/install.sh desktop` installs `loom` and `loom-agent` for the current user, from the x86-64 archive. It also installs these files, so that the application menu shows Loom:

- The desktop entry `dev.loom.Loom.desktop` in `~/.local/share/applications`.
- The icons in `~/.local/share/icons`.

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s desktop
```

### Options

Put the options after the component, for example `| sh -s agent --version 0.2.0`.

| Option | Effect |
| --- | --- |
| `--version <version>` | Installs this release, not the latest release. |
| `--bin-dir <dir>` | Installs the binaries in `<dir>`, not in `~/.local/bin`. |
| `--source` | Builds the binaries with Cargo. Use it on hosts that have no prebuilt binary. Without `--version`, it builds the `main` branch. The desktop client needs the packages in [Build from source](#build-from-source). |
| `--no-config` | Does not change the configuration. |

Prebuilt binaries are available for x86-64 and arm64 Linux. The desktop client is available for x86-64 Linux only.

### After the installation

A running desktop client keeps its old version until you restart it.

## Updates

The desktop client updates itself from the GitHub releases. The update does these steps:

1. It downloads the archive for the machine.
2. It compares the archive with `SHA256SUMS`.
3. It makes sure that each new binary runs.
4. It replaces the binaries in the directory of the running binary.

If a step fails, the old binaries stay.

### Desktop client

The desktop client looks for a newer release when it starts, and then one time each day. When it finds a release, it shows a notification with an **Update** button. The update replaces `loom` and the `loom-agent` in the same directory. Then the client asks if it can restart.

- To look for a release manually, choose **app: check for updates** in the command palette.
- To stop the automatic checks, set `check_for_updates: false` in `client.yaml`.
- `loom update` updates the desktop client and its agent without a window. `--check` only tells if a newer release is available (exit status 1 if so). `--version <version>` installs this release, and can also install an older release.

Debug builds do not look for releases automatically.

### Limits

- The updates do not change the desktop entry or the icons. To update them, run `scripts/install.sh` again.
- The agent, the oracle, and `loom-host` do not update themselves. The agent of a codespace comes from `loom-host`. See `agent.path` in [Hosts and codespaces](host.md#hostyaml).
- The directory of the binaries must be writable. For an installation for all users, run the install script as root.

## Build from source

You need a recent stable Rust toolchain (edition 2024).

The agent, the oracle, and `loom-host` need only Cargo. The desktop client also needs `pkg-config` and the development files for fontconfig, freetype, xkbcommon, and wayland. To run the desktop client, you need these libraries and a Vulkan driver. On Debian or Ubuntu:

```sh
sudo apt install pkg-config libfontconfig-dev libfreetype-dev libxkbcommon-dev libwayland-dev libvulkan-dev
```

Build all binaries:

```sh
cargo build --release
```

Or install the binaries in `~/.cargo/bin`:

```sh
cargo install --path crates/loom-agent
cargo install --path crates/loom-gui      # installs `loom`
cargo install --path crates/loom-oracle
cargo install --path crates/loom-host
```

`scripts/install.sh agent --source` also builds the agent with Cargo.

To build the Android app, see [Android app](android.md#build).
