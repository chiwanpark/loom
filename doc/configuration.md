# Agent configuration

## Configuration file

The agent runs in a codespace. It writes its `agent.yaml` from the preset of the codespace. See [Presets](oracle.md#presets).

Outside a codespace, `loom-agent config` reads `~/.config/loom/agent.yaml`. To use a different file, use `--config <path>` or `LOOM_CONFIG`. Use these commands to write and validate a preset:

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
| `llm` | The default model and the provider settings. See [Model providers](#model-providers). In a preset, `accounts` gives the model account of each provider. See [Presets](oracle.md#presets). |
| `tools` | The settings of the tools. See [Tools](#tools). |
| `tools.comment_guard` | Stops the model when it adds comments to code. See [Comment guard](comment-guard.md). |
| `policy` | Tells if the agent runs a tool call, refuses it, or asks for your approval. See [Policy and harmful tool calls](policy.md). |
| `environment` | The environment variables of shell commands and terminals, and the env file with secrets. See [Environment variables and secrets](environment.md). In a preset, `secrets` gives the secret of the oracle for each variable. |
| `git` | `credentials` names the Git credentials of the oracle that a codespace can use. See [Presets](oracle.md#presets). |
| `sandbox` | Confines shell commands with `landlock` or `bubblewrap`. It is off by default. It can also block the network and add writable paths. Changes in the writable paths are not [harmful](policy.md#harmful-tool-calls), also when the sandbox is off. |
| `context` | The limits of tool output, and when to compact a long conversation. |
| `limits` | See [Limits](#limits). |
| `instructions` | More instructions for the model. The agent also adds the `AGENTS.md` files of the workspace. |
| `skills` | The settings of [skills](skills.md). |
| `telemetry` | OpenTelemetry (OTLP) export of metrics and events. It is off by default. See [Telemetry](telemetry.md). |

The agent of a codespace sets `allowed_roots`, `socket`, `storage`, `lifecycle`, and `relay` itself.

### Model providers

| Provider | Sign-in |
| --- | --- |
| `anthropic` | Claude Pro/Max account (browser) |
| `github-copilot` | GitHub.com account with Copilot access (device code) |
| `openai` | ChatGPT account (browser) or API key (`OPENAI_API_KEY`) |
| `openai-codex` | ChatGPT account (browser or device code) |
| `opencode-go` | [OpenCode Go](https://opencode.ai/docs/console/go/) API key (`OPENCODE_API_KEY`) |
| Any OpenAI-compatible server | API key. Configure it in `llm.providers`. |

Add and sign in to model accounts on the **Models** page of the oracle settings in the desktop client, or with `loom-oracle-cli sign-in` and `loom-oracle-cli set-api-key`. Select the account for each provider in `llm.accounts` of the preset. See [Model accounts](oracle.md#model-accounts).

The `anthropic` provider uses message threads by default on Anthropic's API. When the history and request settings still match, Loom sends only the new messages and the billing block.
If the thread expires, Loom sends the full history again.
To turn off threads, set `llm.providers.anthropic.message_threads: false`.
Threads are off by default for custom base URLs. Set `message_threads: true` only if the endpoint supports the Anthropic message-threads beta.

For GitHub Copilot, sign in with a device code and enter the code on the GitHub page that opens.
After sign-in, select a Copilot model in the model picker.
To make it the default, set `llm.default_model` to `github-copilot/<model-id>` with a model ID from the picker.
Loom does not change which models your account or organization enables.

For OpenCode Go, add an API key account from a Go workspace on the **Models** page of the oracle settings (or with `loom-oracle-cli set-api-key`), or set `OPENCODE_API_KEY` in the environment. The environment variable takes precedence over the stored key. Select an OpenCode Go model in the model picker, or set `llm.default_model` to a model such as `opencode-go/deepseek-v4.1-flash`.

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

A running agent reads `agent.yaml` again when the file changes. For example, the oracle changes the file when you change the preset of the codespace.

The agent tells each connected client if it reloaded the file or if it rejected the file.

If an agent rejects the file, it keeps its current configuration. Each connected client shows the reason.

A new session gets its model, reasoning effort, policy action, and comment guard from `llm.default_model`, `llm.default_reasoning_effort`, `policy.default`, and `tools.comment_guard.mode`. A reload does not change these settings in the sessions that exist.

Changes to `telemetry` apply only after the agent restarts. The agent shows the sections that wait for a restart.

## Agent commands

| Command | Effect |
| --- | --- |
| `loom-agent serve --managed` | Runs the agent of a codespace. `loom-host` starts it in the codespace. See [Codespace agents](host.md#codespace-agents). |
| `loom-agent stdio --managed` | Connects stdin and stdout to the agent of the codespace. SSH connections to a codespace use this command. |
| `loom-agent config init \| check \| schema \| path \| fetch-telemetry` | Manages the configuration. |

Loom no longer has the commands `ps`, `stop`, `reload`, `pair`, `clients`, `host`, and `update`, and `serve` and `stdio` without `--managed`.
