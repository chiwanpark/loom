# Agent configuration

## Configuration file

The agent reads `~/.config/loom/agent.yaml`. To use a different file, use `--config <path>` or `LOOM_CONFIG`.

```sh
loom-agent config init     # write ~/.config/loom/agent.yaml
loom-agent config path     # show the file that the agent uses
loom-agent config check    # validate the file
loom-agent config schema   # show the JSON Schema
```

`crates/loom-config/tests/fixtures/agent.yaml` is an example. It uses most of the settings.

## Sections

| Section | Contents |
| --- | --- |
| `allowed_roots` | The directories that an agent can serve as a workspace. The default is `~`. |
| `llm` | The default model and the provider settings. See [Model providers](#model-providers). |
| `tools` | The settings of the tools. See [Tools](#tools). |
| `tools.comment_guard` | Stops the model when it adds comments to code. See [Comment guard](comment-guard.md). |
| `policy` | Tells if the agent runs a tool call, refuses it, or asks for your approval. See [Policy and harmful tool calls](policy.md). |
| `environment` | The environment variables of shell commands and terminals, and the env file with secrets. See [Environment variables and secrets](environment.md). |
| `worktrees` | `dir` is the directory of the git worktrees of sessions. The default is `~/.config/loom/worktrees`. See [Sessions in a worktree](sessions.md#sessions-in-a-worktree). |
| `sandbox` | Confines shell commands with `landlock` or `bubblewrap`. It is off by default. It can also block the network and add writable paths. Changes in the writable paths are not [harmful](policy.md#harmful-tool-calls), also when the sandbox is off. |
| `context` | The limits of tool output, and when to compact a long conversation. |
| `limits` | See [Limits](#limits). |
| `instructions` | More instructions for the model. The agent also adds the `AGENTS.md` files of the workspace. |
| `skills` | The settings of [skills](skills.md). |
| `relay` | The settings of the relay connection. See [Relays](remote-hosts.md#relays). |
| `telemetry` | OpenTelemetry (OTLP) export of metrics and events. It is off by default. See [Telemetry](telemetry.md). |

### Model providers

| Provider | Sign-in |
| --- | --- |
| `anthropic` | Claude Pro/Max account (browser) |
| `openai` | ChatGPT account (browser) or API key (`OPENAI_API_KEY`) |
| `openai-codex` | ChatGPT account (browser or device code) |
| `opencode-go` | [OpenCode Go](https://opencode.ai/docs/console/go/) API key (`OPENCODE_API_KEY`) |
| Any OpenAI-compatible server | API key. Configure it in `llm.providers`. |

To sign in, use **Providers and sign-in…** in the menu of the agent.

The `anthropic` provider uses message threads by default on Anthropic's API. When the history and request settings still match, Loom sends only the new messages and the billing block.
If the thread expires, Loom sends the full history again.
To turn off threads, set `llm.providers.anthropic.message_threads: false`.
Threads are off by default for custom base URLs. Set `message_threads: true` only if the endpoint supports the Anthropic message-threads beta.

The agent keeps the credentials in `credentials.json` in its data directory. The data directory is `storage.data_dir`. The default is `~/.local/share/loom`.

For OpenCode Go, paste a Console API key from a Go workspace in **Providers and sign-in…**, or set `OPENCODE_API_KEY` in the agent's environment. The environment variable takes precedence over the stored key. Select an OpenCode Go model in the model picker, or set `llm.default_model` to a model such as `opencode-go/deepseek-v4.1-flash`.

This example adds an OpenAI-compatible server, such as a local model:

```yaml
llm:
  default_model: local/qwen3-coder
  providers:
    local:
      type: openai-compatible
      base_url: http://127.0.0.1:11434/v1
      api: chat
      models: [qwen3-coder]
```

`llm.default_reasoning_effort` sets the reasoning effort of new sessions. The values are `none`, `minimal`, `low`, `medium`, `high`, and `xhigh`. The default is `medium`. You can select a different effort for each session.

### Tools

| Setting | Meaning |
| --- | --- |
| Output limits | The output limits of the `read` and `shell` tools. |
| Shell | The shell program of the `shell` tool. |
| `search_web` | The model, the source limits, and the timeout of the `search_web` tool. |
| `notes.max_bytes` | The maximum size of the [notes](sessions.md#notes-and-tasks). The default is 64 KiB. |
| `tasks.max_items` | The maximum number of tasks in a task list. The default is 100. |
| `allowed_dirs` | Directories outside the workspace for the `read`, `edit`, and `write` tools. |

The `search_web` tool uses an OpenAI model to search the web. It needs the `openai-codex` sign-in. Without this sign-in, the model cannot use the tool.

`allowed_dirs` lets the `read`, `edit`, and `write` tools read and change files outside the workspace, for example in `~/notes`. Each path must be absolute or start with `~`. The system prompt shows these directories. The policy still applies to each tool call. The sandbox of shell commands does not change.

### Limits

`limits.max_rounds_per_turn` is the maximum number of model calls for one message. When a turn gets to this limit, it stops. The default is 200. `0` sets no limit.

This limit stops a model that is in a loop when nobody looks at it. The agent keeps the conversation. To continue, send a new message.

## Reload the configuration

A running agent reads `agent.yaml` again when the file changes. You can also make it read the file manually. For example, do this when the agent did not see a change, or when you want to know if the agent accepted the file.

- Run `loom-agent reload --workspace <dir> | --instance <id> | --all`.
- Or, in the Sessions panel of the client, choose **Reload configuration** in the menu of an agent under **Remotes**.

Each agent tells if it reloaded the file, if the file did not change, or if it rejected the file. `loom-agent reload` exits with status 1 if an agent rejected the file or if it could not connect to an agent.

If an agent rejects the file, it keeps its current configuration. Each connected client shows the reason.

A new session gets its model, reasoning effort, policy action, and comment guard from `llm.default_model`, `llm.default_reasoning_effort`, `policy.default`, and `tools.comment_guard.mode`. A reload does not change these settings in the sessions that exist.

Changes to `relay`, `socket`, `storage`, `lifecycle`, and `telemetry` apply only after the agent restarts. The agent shows the sections that wait for a restart.

## Agent commands

| Command | Effect |
| --- | --- |
| `loom-agent serve --workspace <dir>` | Runs an agent for a workspace. `--default` serves `lifecycle.default_workspace`. |
| `loom-agent ps [--json]` | Shows the agents that run on this host. |
| `loom-agent stop --workspace <dir> \| --instance <id> \| --all` | Stops agents gracefully. |
| `loom-agent reload --workspace <dir> \| --instance <id> \| --all` | Makes agents read their configuration file again. See [Reload the configuration](#reload-the-configuration). |
| `loom-agent stdio --workspace <dir> [--spawn]` | Connects stdin and stdout to an agent. SSH connections use this command. `--spawn` starts the agent if it does not run. |
| `loom-agent pair` | Shows a one-time pairing link for a relay client. |
| `loom-agent clients list \| revoke <label>` | Manages the clients that are paired through a relay. |
| `loom-agent host [--json]` | Shows the ID and the public key of this host. |
| `loom-agent config init \| check \| schema \| path \| fetch-telemetry` | Manages the configuration. |
| `loom-agent update [--check] [--version <version>] [--restart]` | Updates `loom-agent`. See [Updates](install.md#updates). |
