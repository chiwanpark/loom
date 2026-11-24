# Environment variables and secrets

The `environment` section of `agent.yaml` sets the environment variables of shell commands and terminals.

## Order of the rules

The agent applies these rules in this order:

1. It starts with the environment of the agent.
2. `inherit` selects the variables to keep:
   - `all` keeps all variables. This is the default.
   - `core` keeps only `HOME`, `LOGNAME`, `PATH`, `SHELL`, `USER`, `USERNAME`, the temporary directories, the locale, and `TZ`.
   - `none` keeps no variables.
3. The default excludes remove the variables with `KEY`, `SECRET`, `TOKEN`, or `PASSWORD` in their names. To keep these variables, set `ignore_default_excludes: true`.
4. `exclude` removes the variables that match its name patterns.
5. `env_file` adds the variables of the env file. The default is `.env` in the workspace. `null` sets no env file.
6. `set` adds variables with fixed values.
7. If `include_only` is not empty, only the variables that match it stay.

In patterns, `*` matches any text and `?` matches one character. Patterns ignore case.

## Credentials in the env file

Put the credentials of CLIs, such as `aws` or `lakectl`, in the env file of the workspace:

```sh
# .env
LAKECTL_CREDENTIALS_ACCESS_KEY_ID=AKIA...
LAKECTL_CREDENTIALS_SECRET_ACCESS_KEY=...
AWS_REGION=eu-west-1
```

To let the model see a value, add the variable to `redaction.allow`:

```yaml
environment:
  redaction:
    allow: [AWS_REGION]
```

When the env file changes, the agent reads it again. If the agent cannot parse the file, commands keep the old values. The agent shows a warning with the line number.

## Redaction

The model does not get the values of the variables from the env file and from `set`. The variables in `redaction.allow` are an exception.

- The system prompt shows only the names of the variables.
- In tool output, the values show as `[REDACTED:<NAME>]`. This also applies to the base64 and URL-encoded forms of the values.
- The agent also hides the values of its own variables that have secret names. It does this also when commands do not get these variables.
- The agent hides the values before it records tool output, saves it to a file, or sends it to clients and the model.

The agent also protects the env file:

- The `read` tool shows the env file with hidden values.
- The `write` and `edit` tools cannot change the env file. Only you can change it.
- With the `bubblewrap` sandbox, commands cannot read the env file.

The agent shows warnings in these conditions:

- A value has fewer than 8 characters. The agent cannot hide short values.
- Git does not ignore the env file.

The warnings show above the system prompt. To see them, choose **System prompt** in the menu of a session.

Terminals get the same variables. The agent does not hide their output, because only you see it. The files that you open in a client also show the raw env file.

To turn off the redaction and the protection of the env file, set `redaction.enabled: false`.

## Limits of the redaction

The redaction does not find all secrets. For example, a command can show a value in reverse order, or send it over the network. Thus, follow these rules:

- Give the agent credentials with the least privileges.
- If commands do not need the network, set `sandbox.network: false`.

## Environment in a codespace

In a codespace, the systemd service of the agent loads `/etc/loom/environment`, which comes from the environment configuration of the host. The secrets of the preset go into `~/.local/share/loom/secrets.env`, which the agent reads as its env file. See [Presets](oracle.md#presets) and [The codespace image](host.md#the-codespace-image).
