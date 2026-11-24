<p><img src="assets/logo.svg" alt="Loom logo" width="128" height="128"></p>

# Loom

Loom is a coding agent with these parts:

- An **oracle** keeps your workspaces, presets, model accounts, and secrets. It is also the relay between the clients and the agents.
- A **host** runs codespaces. A codespace is an Incus container with a copy of the repositories of a workspace.
- An **agent** runs in each codespace. It talks to the model, reads and edits files, and runs shell commands.
- A **client** connects to the agents through the oracle and shows their sessions, terminals, file changes, and diffs. The desktop client is `loom`. There is also an [Android app](doc/android.md).

Agents continue to run when the client disconnects. You can open the client again later, also on a different device, and continue the same sessions.

Loom is early software. The configuration formats and the protocol can change between 0.x versions. [CHANGELOG.md](CHANGELOG.md) shows the changes in each release.

## Components

| Binary | Purpose |
| --- | --- |
| `loom-oracle` | The oracle. See [Oracle](doc/oracle.md). |
| `loom-host` | Runs the codespaces of a host. See [Hosts and codespaces](doc/host.md). |
| `loom-agent` | The agent of a codespace. |
| `loom` | The desktop client. Linux and Wayland only. |
| Loom for Android | The Android client. Android 17 (API 37) or newer, arm64. |

## Install

Install the desktop client on your machine:

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s desktop
```

The prebuilt binaries of the oracle, `loom-host`, and the agent are on the [releases page](https://github.com/chiwanpark/loom/releases). For other options, updates, and builds from source, see [Installation and updates](doc/install.md).

## Get started

1. Set up an oracle. See [Set up an oracle](doc/oracle.md#set-up-an-oracle).
2. Start the desktop client with `loom`. Use **Pair with an oracle** and paste the pairing link of the oracle. See [Add an oracle](doc/desktop.md#add-an-oracle).
3. Pair a host with the oracle and start `loom-host` on it. See [Hosts and codespaces](doc/host.md).
4. In the oracle settings of the client, add a model account, a preset, and a workspace with its repositories.
5. Under **Workspaces**, click **+** next to the workspace to start a codespace with a first message.

Loom no longer has local agents, agents over SSH, `loom-relay`, or sessions in a git worktree. Each session runs in a codespace.

## Documentation

| Document | Contents |
| --- | --- |
| [Installation and updates](doc/install.md) | Release archives, the install script, updates, and builds from source. |
| [Oracle](doc/oracle.md) | The oracle, its relay and control service, presets, model accounts, secrets, and notifications. |
| [Hosts and codespaces](doc/host.md) | `loom-host`, its Incus image, network rules, Git workspaces, repository setup, and persistent volumes. |
| [Agent configuration](doc/configuration.md) | `agent.yaml`, model providers, reload, and agent commands. |
| [Policy and harmful tool calls](doc/policy.md) | Tool approval rules and the harmful check. |
| [Environment variables and secrets](doc/environment.md) | The environment of commands, the env file, and redaction. |
| [Comment guard](doc/comment-guard.md) | Stop the model when it adds comments to code. |
| [Telemetry](doc/telemetry.md) | OpenTelemetry export of metrics and events. |
| [Sessions](doc/sessions.md) | Session states, queue and steer, rewind, notes, and tasks. |
| [Skills](doc/skills.md) | Instructions for special tasks in the Agent Skills format. |
| [Desktop client](doc/desktop.md) | `client.yaml`, key bindings, panels, codespaces, oracle settings, terminals, files, and ports. |
| [Android app](doc/android.md) | Build, pairing, codespaces, and the functions of the app. |
| [Development](doc/development.md) | Crates and checks. |

## License

Loom is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).

Some code comes from [Zed](https://github.com/zed-industries/zed). Some themes come from [gpui-component](https://github.com/longbridge/gpui-component). Gotham is based on [vim-gotham](https://github.com/whatyouhide/vim-gotham). The other themes come from the palettes at [TerminalColors](https://terminalcolors.com/). [NOTICE](NOTICE) shows the details and the authors of the color schemes.
