<p><img src="assets/logo.svg" alt="Loom logo" width="128" height="128"></p>

# Loom

Loom is a coding agent with two parts:

- An **agent** runs on the machine with the code. It talks to the model, reads and edits files, and runs shell commands.
- A **client** connects to agents and shows their sessions, terminals, file changes, and diffs. The desktop client `loom` connects to agents on your machine, over SSH, or through a relay. The [Android app](doc/android.md) connects through a relay.

Agents continue to run when the client disconnects. You can open the client again later, also on a different machine, and continue the same sessions.

Loom is early software. The configuration formats and the protocol can change between 0.x versions. [CHANGELOG.md](CHANGELOG.md) shows the changes in each release.

## Components

| Binary | Purpose |
| --- | --- |
| `loom-agent` | The agent daemon and its commands. One agent serves one workspace directory. |
| `loom` | The desktop client. Linux and Wayland only. |
| `loom-relay` | An optional WebSocket relay. Clients use it to connect to agents behind NAT without SSH. |
| Loom for Android | An Android client for agents behind a relay. Android 17 (API 37) or newer, arm64. |

## Install

Install the desktop client and the agent on your machine:

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s desktop
```

Install only the agent, for example on a remote host:

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s agent
```

The prebuilt binaries are on the [releases page](https://github.com/chiwanpark/loom/releases). For other options, updates, and builds from source, see [Installation and updates](doc/install.md).

## Get started

1. Make the agent configuration:

   ```sh
   loom-agent config init     # writes ~/.config/loom/agent.yaml
   ```

2. Start the client with `loom`. It shows the hosts from `~/.config/loom/client.yaml`. By default, there is only your own machine (`local`).

3. Start an agent for a project directory. In the Sessions panel, click **+** next to a host under **Remotes** and type the directory. Or start the agent yourself:

   ```sh
   loom-agent serve --workspace ~/src/my-project
   ```

4. In the menu of the agent, choose **Providers and sign-in…** and sign in to a [model provider](doc/configuration.md#model-providers), such as OpenCode Go.
   For GitHub Copilot, choose **Device code** and complete sign-in on GitHub.

5. Press `ctrl-n` or click **+** next to the host to start a session.

The client finds running agents automatically. For the local host, it starts the `loom-agent` in the directory of `loom`, or the `loom-agent` on `PATH`. To use a different binary, set `LOOM_AGENT_BIN`.

When you sign in with Claude Pro or Max, Loom uses message threads to avoid uploading the full conversation on every request. See [Model providers](doc/configuration.md#model-providers) for the settings.

To connect to agents on other machines, see [Remote hosts and relays](doc/remote-hosts.md).

## Documentation

| Document | Contents |
| --- | --- |
| [Installation and updates](doc/install.md) | Release archives, the install script, updates, and builds from source. |
| [Remote hosts and relays](doc/remote-hosts.md) | SSH hosts, relays, pairing, and agents as systemd services. |
| [Agent configuration](doc/configuration.md) | `agent.yaml`, model providers, reload, and agent commands. |
| [Policy and harmful tool calls](doc/policy.md) | Tool approval rules and the harmful check. |
| [Environment variables and secrets](doc/environment.md) | The environment of commands, the env file, and redaction. |
| [Comment guard](doc/comment-guard.md) | Stop the model when it adds comments to code. |
| [Telemetry](doc/telemetry.md) | OpenTelemetry export of metrics and events. |
| [Sessions](doc/sessions.md) | Session states, queue and steer, rewind, worktrees, notes, and tasks. |
| [Skills](doc/skills.md) | Instructions for special tasks in the Agent Skills format. |
| [Desktop client](doc/desktop.md) | `client.yaml`, key bindings, panels, codespaces, oracle settings, terminals, files, and ports. |
| [Android app](doc/android.md) | Build, pairing, codespaces, and the functions of the app. |
| [Oracle](doc/oracle.md) | Loom-Next, in development: the oracle as a relay, its control service, presets, model accounts, secrets, and the test client. |
| [Hosts and codespaces](doc/host.md) | Loom-Next, in development: `loom-host`, its Incus image, network rules, Git workspaces, repository setup, and persistent volumes. |
| [Development](doc/development.md) | Crates and checks. |

## License

Loom is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).

Some code comes from [Zed](https://github.com/zed-industries/zed). Some themes come from [gpui-component](https://github.com/longbridge/gpui-component). Gotham is based on [vim-gotham](https://github.com/whatyouhide/vim-gotham). The other themes come from the palettes at [TerminalColors](https://terminalcolors.com/). [NOTICE](NOTICE) shows the details and the authors of the color schemes.
