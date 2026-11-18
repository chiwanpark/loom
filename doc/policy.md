# Policy and harmful tool calls

The `policy` section of `agent.yaml` tells the agent what to do with each tool call.

## Actions

| Action | Effect |
| --- | --- |
| `ask` | The agent asks for your approval of each call. This is the default. For a call that can be harmful, the request shows the reason. |
| `allow` | The agent runs the call without approval. If the call can be harmful, the agent asks for your approval. |
| `allow_harmful` | The agent runs the call without approval and without the harmful check. |
| `deny` | The agent refuses the call. |

## Rules

Rules can match a tool and a command prefix. For example, a rule can allow `git status` and another rule can deny `sudo`.

```yaml
policy:
  default: ask
  rules:
  - tool: read
    action: allow
  - tool: shell
    command_prefix: ["git status", "git diff", "cargo check", "cargo test"]
    action: allow
  - tool: shell
    command_prefix: ["rm -rf /", "sudo"]
    action: deny
```

The agent allows the notes, tasks, and `skill` tools, unless a rule tells otherwise.

Rules can also use `allow_harmful`, for example to allow `git push --force` in a workspace.

You can also select an action for one session in the desktop app and in the Android app. This includes **Allow harmful**.

**Allow for session** on the approval of a call that can be harmful adds an `allow` rule. Thus, the agent asks again for the next call that can be harmful.

## Harmful tool calls

When the policy allows a `shell`, `edit`, or `write` call, the agent first examines the call. If the call can be harmful, the agent asks for your approval and shows the reason.

These calls can be harmful:

- `rm`, `rmdir`, `unlink`, `shred`, `mv`, `cp`, `chmod`, `chown`, `chgrp`, `tee`, `touch`, `truncate`, `dd`, and `ln` on a path outside the workspace.
- Output redirects (`>` and `>>`) to a path outside the workspace.
- `find -delete`, `find -exec rm`, `xargs rm`, and `rsync --delete` on a path outside the workspace.
- Changes to the `.env` files and to the `.git` directory at the root of the workspace. Changes to `.env.example` are not harmful.
- Destructive git commands: `git checkout --`, `git restore`, `git reset --hard`, `git reset --merge`, `git push --force`, `git clean -f`, `git branch -D`, `git stash drop`, and `git stash clear`. `git restore --staged` is not harmful.
- `edit` and `write` calls on a protected path or a denied path.

These changes are not harmful:

- Changes in the temporary directories: `/tmp`, `/var/tmp`, and `$TMPDIR`.
- Changes in `sandbox.writable`.
- Changes by the `edit` and `write` tools in `tools.allowed_dirs`. Changes by shell commands in these directories are harmful.
- Changes to `/dev/null` and to the standard streams. The deletion of these files is harmful.

### Paths

```yaml
policy:
  default: allow
  harmful:
    deny_paths: [./infra/production]  # paths whose changes are always harmful
sandbox:
  writable: [~/.cargo, ~/scratch]    # paths whose changes are not harmful
```

The paths in `sandbox.writable` must be absolute or start with `~`. In a writable path, changes to `.env` and `.git` paths and the deletion of `/dev/null` are not harmful.

The paths in `deny_paths` can be absolute, start with `~`, or be relative to the workspace. `deny_paths` has priority over `sandbox.writable` and over the workspace. Changes to the parent directories of a denied path are also harmful. This also applies to glob patterns that can include a denied path.

### How the check works

- The check examines each command in `&&`, `||`, `;`, `|`, and newline chains.
- It follows `cd`, the variables that the command set before, and the environment of commands.
- It resolves symbolic links. This also applies to files that do not exist yet.
- It does not examine heredoc bodies, quoted text, and shell comments.
- If it cannot find the path of a target, such as `$(...)`, the call can be harmful.

The check is not a sandbox. It cannot find all harmful commands, for example commands in a script file. To confine commands, also use `sandbox`.
