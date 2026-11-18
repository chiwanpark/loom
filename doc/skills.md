# Skills

Skills give the agent instructions for special tasks. They use the [Agent Skills](https://agentskills.io) format.

## Skill format

A skill is a directory with a `SKILL.md` file:

- The file starts with YAML frontmatter. The frontmatter has the `name` and the `description` of the skill.
- After the frontmatter, the file has the instructions in Markdown.
- The directory can also have other files, such as references and scripts.

The agent skips a skill without a description or without frontmatter. It shows the reason as a warning. The agent ignores other fields of the frontmatter, such as `allowed-tools`.

## Skill directories

The agent looks for skills in these directories, in this order:

1. `.loom/skills/` and `.agents/skills/` in the workspace.
2. `~/.config/loom/skills/` and `~/.agents/skills/`.
3. The directories in `skills.paths` in `agent.yaml`. A relative path is relative to the workspace. A directory can contain skills, or it can be one skill.

If two skills have the same name, the agent uses the first skill that it finds. Thus, a skill of the workspace replaces a skill of the user.

The agent reads the directories before each model call. New and changed skills apply immediately.

## How the model uses skills

1. The system prompt shows the name and the description of each skill.
2. When a task matches a skill, the model calls the `skill` tool with the name of the skill.
3. The tool returns the instructions without the frontmatter, the directory of the skill, and a list of its other files.

The list does not include hidden files and files that tools make: `__pycache__`, `node_modules`, Python virtual environments, `*.pyc`, `*.pyo`, `*~`, and `Thumbs.db`.

The `read` tool can read the files in the directory of each listed skill, also outside the workspace. The other file tools cannot change these files.

The model runs the scripts of a skill with the `shell` tool. The normal policy applies.

When the agent compacts the conversation, the instructions of the loaded skills stay in the context.

## Configuration

| Setting | Meaning |
| --- | --- |
| `skills.enabled` | Turns skills on or off. The default is on. |
| `skills.paths` | More directories of skills. |
| `skills.disabled` | The names of skills that the agent does not offer. |

To see the skills of a session, open its system prompt. See [System prompt](sessions.md#system-prompt).
