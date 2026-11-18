# Telemetry

The `telemetry` section of `agent.yaml` makes the agent export metrics and events over OTLP. It is off by default. The data uses the model of the telemetry of Claude Code.

`loom-agent config fetch-telemetry` shows a `telemetry` section from the settings that your organization publishes for Claude Code. It uses the Anthropic sign-in of the agent host.

## Names of Claude Code

To use the same names as Claude Code, set these values:

```yaml
telemetry:
  prefix: claude_code        # metric names such as claude_code.token.usage
  service_name: claude-code  # the service.name resource attribute
```

## User attributes

Each metric and event has the user attributes of Claude Code:

| Attribute | Value |
| --- | --- |
| `user.email` | `telemetry.user_email`. If it is not set, the email of the Anthropic sign-in. If there is no email, `git config --global user.email`. |
| `user.account_uuid` | `telemetry.account_uuid`. If it is not set, the account of the Anthropic sign-in. |
| `user.account_id` | The same account in the format of Claude Code, such as `user_01…`. |
| `organization.id` | `telemetry.organization_id`. If it is not set, the organization of the Anthropic sign-in. |

To remove `user.account_uuid` and `user.account_id`, set `include_account_uuid: false`. `fetch-telemetry` copies this setting from `OTEL_METRICS_INCLUDE_ACCOUNT_UUID`.

A sign-in from Loom 0.4.0 or older gets its organization at its next token refresh.

The agent reads these values when it starts. After you sign in or change the `telemetry` section, restart the agent.
