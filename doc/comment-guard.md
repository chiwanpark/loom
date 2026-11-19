# Comment guard

The comment guard stops the model when it adds comments to code. The `edit` and `write` tools compare each change with the file before the change. Then they find the comments that the change adds.

## Allowed comments

The guard allows these comments:

- Changes to existing comments, for example new words in a comment.
- New comment lines next to an existing comment, for example one more line in a doc comment.
- Comments that the change moves. This also applies to a move from one file to a different file in one patch.
- Tool directives, such as `eslint-disable`, `@ts-expect-error`, `# noqa`, `# type: ignore`, and `#!/usr/bin/env`.

The guard knows the comments of common languages by the file extension, such as `.rs`, `.ts`, `.py`, `.go`, `.sh`, `.yaml`, `.sql`, and `.html`. It does not examine other files.

## Configuration

```yaml
tools:
  comment_guard:
    mode: block              # block (default), warn, or off
    allow_directives: true   # allow tool directives
    allow_patterns: ["TODO\\(#\\d+\\)"]   # regular expressions of comments to allow
    ignore_paths: [vendor, docs/examples] # paths in the workspace not to check
```

| Mode | Effect |
| --- | --- |
| `block` | The tool rejects a change with new comments. It tells the model which comments to remove. |
| `warn` | The tool makes the change. It tells the model about the new comments. |
| `off` | The tool does not examine comments. |

When the guard is on, the system prompt tells the model not to add comments.

## One session

You can turn the guard on or off for one session:

- In the desktop app, use **Comment guard** in the status bar.
- In the Android app, use the comment guard item in the row above the prompt box.

A new session uses the `mode` of `agent.yaml`. The status bar and the row show if the guard is on or off. If `mode` is `off`, **On** uses `block`.
