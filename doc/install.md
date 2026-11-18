# Installation and updates

## Release archives

The [releases page](https://github.com/chiwanpark/loom/releases) has prebuilt archives for Linux.

| Archive | Contents | Requirements |
| --- | --- | --- |
| `loom-<version>-x86_64-unknown-linux-gnu.tar.gz` | The desktop client `loom` and `loom-agent`, for x86-64. | glibc 2.39 or newer (Ubuntu 24.04, Debian 13), `libxkbcommon`, a Wayland session, and a Vulkan driver. |
| `loom-agent-<version>-<arch>-unknown-linux-musl.tar.gz` | `loom-agent`, statically linked. | Any Linux. |
| `loom-relay-<version>-<arch>-unknown-linux-musl.tar.gz` | `loom-relay`, statically linked. | Any Linux. |

`<arch>` is `x86_64` for x86-64 and `aarch64` for arm64.

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

To install the agent on an SSH host, run the script from a checkout:

```sh
ssh devbox sh -s agent < scripts/install.sh
```

### Relay

`scripts/install.sh relay` installs `loom-relay`. As root, the script installs for all users:

- The binary goes in `/usr/local/bin`.
- The configuration goes in `/etc/loom/relay.yaml`.

With `--systemd`, the script also installs and starts the `loom-relay` system service. The service runs the relay as an unprivileged user that systemd makes.

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sudo sh -s relay --systemd
```

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
| `--bin-dir <dir>` | Installs the binaries in `<dir>`, not in `~/.local/bin` or `/usr/local/bin`. |
| `--source` | Builds the binaries with Cargo. Use it on hosts that have no prebuilt binary. Without `--version`, it builds the `main` branch. The desktop client needs the packages in [Build from source](#build-from-source). |
| `--systemd` | Installs and starts the systemd service. For the agent, this is the user service in [Remote hosts and relays](remote-hosts.md#keep-an-agent-running). For the relay, this is the system service, which needs root. The desktop client has no service. |
| `--no-config` | Does not change the configuration. |

Prebuilt binaries are available for x86-64 and arm64 Linux. The desktop client is available for x86-64 Linux only.

### After the installation

A running agent, relay, or desktop client keeps its old version until you restart it. Use `loom-agent stop --all` or `systemctl restart loom-relay`. The script tells you when an agent or a relay must restart. It restarts the services that it installed.

Non-interactive SSH commands often do not have `~/.local/bin` on `PATH`. In this case, set `remote_bin` for the host in `client.yaml`. See [Remote hosts and relays](remote-hosts.md#ssh-hosts).

## Updates

The agent and the desktop client update themselves from the GitHub releases. The update does these steps:

1. It downloads the archive for the machine.
2. It compares the archive with `SHA256SUMS`.
3. It makes sure that each new binary runs.
4. It replaces the binaries in the directory of the running binary.

If a step fails, the old binaries stay.

### Agent

- `loom-agent update` installs the latest release of `loom-agent`.
- `loom-agent update --check` only tells if a newer release is available. If a newer release is available, the exit status is 1.
- `loom-agent update --version <version>` installs this release. It can also install an older release.

Running agents keep their old version until you restart them with `loom-agent stop --all`.

### Desktop client

The desktop client looks for a newer release when it starts, and then one time each day. When it finds a release, it shows a notification with an **Update** button. The update replaces `loom` and the `loom-agent` in the same directory. Then the client asks if it can restart.

- To look for a release manually, choose **app: check for updates** in the command palette.
- To stop the automatic checks, set `check_for_updates: false` in `client.yaml`.
- `loom update` updates the desktop client and its agent without a window. It has the same `--check` and `--version` options.

Debug builds do not look for releases automatically.

### Agents of a host

The desktop client and the Android app can update the agents of a host:

- In the desktop client, click the download button of the host under **Remotes**.
- In the Android app, use **Update agents…** in the menu of the host.

The default version is the version of the client. To install the latest release, clear the version.

An agent of the host replaces its binary with the release. The agent does not have to be connected. Then all agents of the host that use this binary restart at the same time, with their PIDs and arguments. Their running turns stop, and the clients connect to them again. Agents of a different build at the same path keep their version until you restart them. An agent also restarts this way when it receives `SIGUSR1`.

The clients can also update agents that use an older protocol, back to the agents of 0.4.0 (protocol 15). For older agents, use `loom-agent update` on the host.

### Limits

- The updates do not change the desktop entry, the icons, or the systemd units. To update them, run `scripts/install.sh` again.
- The relay does not update itself.
- The directory of the binaries must be writable. For an installation for all users, run the install script as root.

## Build from source

You need a recent stable Rust toolchain (edition 2024).

The agent and the relay need only Cargo. The desktop client also needs `pkg-config` and the development files for fontconfig, freetype, xkbcommon, and wayland. To run the desktop client, you need these libraries and a Vulkan driver. On Debian or Ubuntu:

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
cargo install --path crates/loom-relay    # only if you run a relay
```

Install `loom-agent` on each machine where an agent must run. This includes the SSH hosts. `scripts/install.sh agent --source` also builds the agent with Cargo.

To build the Android app, see [Android app](android.md#build).
