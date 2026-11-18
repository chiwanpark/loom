# Desktop client

The desktop client `loom` runs on Linux with Wayland.

## Client configuration

The client reads `~/.config/loom/client.yaml`. It has these sections:

| Section | Contents |
| --- | --- |
| `hosts` | The hosts of the agents. See [Remote hosts and relays](remote-hosts.md). |
| `appearance` | The theme, the UI and monospace fonts and sizes, and fallback fonts. A fallback font shows the glyphs that the monospace font does not have, for example Hangul in terminals. `wrap_lines` tells if file and diff views wrap long lines when they open. It is off by default. `host_colors` sets the color of each host by its name, for example `devbox: '#3b82f6'`. See [Host colors](#host-colors). |
| `keymap` | Changes to the default key bindings. |
| `check_for_updates` | Tells if the client looks for a newer release when it starts and one time each day. It is on by default. See [Updates](install.md#updates). |

## Key bindings

| Keys | Action |
| --- | --- |
| `ctrl-shift-p` | Command palette |
| `ctrl-n` | New session |
| `ctrl-p` | Find a session |
| `ctrl-alt-o` | Find an agent |
| `ctrl-shift-m` | Select the model |
| `` ctrl-` `` | New terminal |
| `ctrl-b` | Show or hide the Sessions panel |
| `ctrl-shift-b` / `ctrl-shift-n` / `ctrl-shift-t` | Sessions panel / notes of the agent / tasks of the session |
| `enter` / `ctrl-enter` during a turn | Queue the message for the next turn / steer the running turn with it |
| `ctrl-shift-e` / `ctrl-shift-g` | Workspace files / git changes |
| `ctrl-tab` / `ctrl-shift-tab` (or `ctrl-pagedown` / `ctrl-pageup`) | Next / previous tab in the pane |
| `alt-1` … `alt-8` / `alt-9` | Tab 1 to 8 / the last tab in the pane |
| `ctrl-shift-pagedown` / `ctrl-shift-pageup` | Move the tab right / left |
| `ctrl-w` | Close the tab. You cannot close the conversation of the session or the terminal of a different client. |
| `ctrl-k` then an arrow | Split the pane |
| `ctrl-k ctrl-t` | Select a theme |
| `ctrl-s` | Save the file in a file view, or the notes in the Notes tab |
| `alt-z` | Wrap or unwrap long lines in a file or diff view |

The tab and panel keys also work in terminals. Thus, a terminal program does not get them, for example `alt-1` in readline. `ctrl-w`, `ctrl-b`, and `ctrl-p` go to the terminal.

## Sessions panel

The Sessions panel puts the sessions of the connected agents into groups by [state](sessions.md#session-states):

- **Needs approval**: waiting for approval.
- **Working**: thinking, responding, or running a tool.
- **Completed**.
- **Idle**.
- **Archived**.

Under **Remotes**, the panel shows each host and its agents. For each agent, it shows the workspace, the number of sessions, and the connection status.

- To connect to an agent, click it.
- The menu of an agent has the actions for the agent, such as **Stop agent**.

### Host colors

You can give each host a color. The color helps you to identify the sessions of each host.

1. Under **Remotes**, click the palette button of the host.
2. Select a color.

The panel then shows the name of the host in this color. The name shows in each session of the host and under **Remotes**. The client keeps the colors in `appearance.host_colors` of `client.yaml`.

```yaml
appearance:
  host_colors:
    devbox: '#3b82f6'
```

Each color must have the format `#rrggbb`. A host without a color has no mark.

### Start a session

1. Click **+** next to a host. The **New session** dialog shows the workspaces of the agents on the host.
2. Do one of these steps:
   - Click a workspace to start a session there.
   - Click **In a worktree…** to start a session in a new worktree. See [Sessions in a worktree](sessions.md#sessions-in-a-worktree).
   - Type a different directory and click **Start**. The host starts an agent for the directory. The session opens when the client connects to the agent.

### Open and close a session

The conversation tab of a session stays while the session is open. To close a session and all its tabs, use **session: close** in the command palette. The session stays on the agent, and you can open it again.

When you open a session, the client loads its latest 50 messages and tool calls. When you scroll to the top, it loads earlier items. The **Changes** panel loads the full session, to show all files that the session changed.

Each code block in an answer has a copy button. It copies the code without the fences. Each table has a copy button below it. It copies the table as Markdown. The Android app has the same buttons.

## Terminals

A terminal is open on one client at a time. Each client shows the terminals of the agent.

A terminal that a different client opened, such as the Android app, gets a tab in the active session of the agent. The title of the tab shows where the terminal is open. The tab shows the terminal dimmed.

- To move the terminal to this client, press **Take over**. The client gets the recent output, up to 128 KiB. The other client can take it back the same way.
- You cannot close this tab before you take over the terminal, because a closed tab stops the shell of the terminal.
- When you close the session of the tab, the tab moves to a different open session of the agent.

## Files

### File viewer

Images (PNG, JPEG, WebP, GIF, BMP, TIFF, and icons) and PDF files up to 64 MB open in a viewer, not in the text editor. SVG files open as text.

The toolbar of the viewer has these functions:

- Zoom in and zoom out.
- Fit the image to the view, or fit the pages to the width.
- Show the image at its own size.

The viewer draws the pages of a PDF file when they scroll into view.

### Download files

To copy a workspace file to your machine, do one of these steps:

- In the files panel, right-click the file and choose **Download…**.
- Click the download button in the file view.

Downloads work over all connections, also through a relay.

### Upload files

To copy files from your machine to the workspace, do one of these steps:

- Drag the files onto a folder in the files panel. If you drop the files on a file, they go into the folder of this file.
- In the files panel, right-click a folder and choose **Upload files…**.
- Click the upload button above the panel. The files go into the selected folder.

If the folder has a file with the same name, the client asks before it replaces the file. A file shows in the workspace only after all of it arrives. A cancelled or failed upload does not leave files.

Uploads work over all connections, also through a relay. The Android app cannot upload files.

## Changes panel

The **Changes** panel shows the uncommitted changes in git. It also shows the commits that the base branch does not have, and the files that these commits change.

- For a session in a worktree, the base is the base branch of the worktree.
- For other sessions, the base is the upstream branch of the current branch, for example `origin/main`. If there is no upstream branch, the panel shows no commits.

To see the diff of a file from the base to `HEAD`, click the file under **Committed since**. The diff opens in a new tab.

The Android app shows only the uncommitted changes.

## Ports

The **Ports** tab of the right panel shows the TCP ports on the agent host where programs of the agent user listen. To open it, you can also choose **panel: ports** in the command palette.

To forward a local port of this machine to a port of the agent host, do one of these steps:

- Click **Forward** on a port.
- Type a port or `host:port` in the field.

The forward uses the same port number if it is free on this machine. If not, it uses a different port. Each forward shows its local address, with buttons to open it in the browser, copy it, and stop it.

Forwards work over all connections, also through a relay. They continue after the client connects to the agent again.

The agent finds listening ports on Linux only. On other systems, type the port.
