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
| `ctrl-alt-n` | New codespace |
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

The Sessions panel puts the sessions of the connected agents and of the codespaces into groups by [state](sessions.md#session-states):

- **Needs approval**: waiting for approval.
- **Working**: thinking, responding, or running a tool.
- **Completed**.
- **Idle**.
- **Archived**.

Under **Remotes**, the panel shows each host and its agents. For each agent, it shows the workspace, the number of sessions, and the connection status.

- To connect to an agent, click it.
- The menu of an agent has the actions for the agent, such as **Stop agent**.

Under **Workspaces**, the panel shows each oracle with its workspaces and their codespaces. See [Codespaces](#codespaces).

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
   - Type a different directory and click **Start**. The host starts an agent for the directory. The session opens when the client connects to the agent.

### Open and close a session

The conversation tab of a session stays while the session is open. To close a session and all its tabs, use **session: close** in the command palette. The session stays on the agent, and you can open it again.

When you open a session, the client loads its latest 50 messages and tool calls. When you scroll to the top, it loads earlier items. The **Changes** panel loads the full session, to show all files that the session changed.

Each code block in an answer has a copy button. It copies the code without the fences. Each table has a copy button below it. It copies the table as Markdown. The Android app has the same buttons.

## Codespaces

Loom-Next is in development. An oracle keeps workspaces, presets, and codespaces. See [Oracle](oracle.md) and [Hosts and codespaces](host.md).

### Add an oracle

1. Get a pairing link. For the first device, run `loom-oracle pair` on the oracle machine. For other devices, click **Add device…** on the **Devices** page of the oracle settings on a paired device, or run `loom-oracle-cli pair-device`.
2. In the desktop client, use **Pair through a relay**.
3. Type a name and paste the link.

The client keeps the oracle in `client.yaml` as a host of the kind `oracle`:

```yaml
hosts:
- kind: oracle
  name: next
  url: wss://oracle.example.com
  host_id: 3b1f…
  host_key: Qm9i…
```

The client then shows the oracle under **Workspaces**. For each workspace, it shows the host, the repositories, and the codespaces. For each codespace, it shows the branch, the preset, the status, and the number of sessions. The lists come from the oracle. The client connects to the agent of a codespace only when you open a session.

### New codespace

1. Click **+** next to a workspace. Or press `ctrl-alt-n` and select a workspace. If no agent is connected, `ctrl-n` also opens the list of workspaces.
2. Select the preset. The dialog starts with the preset and the limits of the last codespace of the workspace. It shows the host of the workspace and the model accounts of the preset. It shows a warning for each repository that has no Git credential in the preset.
3. Set the CPU limit and the memory limit, for example `4 GiB`.
4. Optional: type a base branch for each repository. An empty field uses the base branch of the workspace, or the default branch of the repository.
5. Optional: type the first message and select its model. The agent starts a session with this message when the codespace is ready.
6. Click **Create**.

A progress view shows the steps of the start. When the codespace is ready, click **Open**. The client opens the newest session of the codespace, or a new session.

### Codespace menu

The **…** button of a codespace opens its menu:

- **New session** adds a session in the codespace.
- **Open pull request…** asks for a title, a body, and the draft mark. For each repository with commits, the agent pushes the branch and opens a pull request to the base branch. The client opens the first pull request in the browser.
- **Show start progress** opens the progress view of a codespace that starts or that failed.
- **Stop codespace…** stops the codespace. Its sessions and terminals end, and its files stay.
- **Resume codespace…** starts a stopped codespace again and shows the start progress. A click on a stopped codespace also does this.
- **Close codespace…** shows the close guard. For each repository with unsaved work, select **Push** or **Discard**. A push of changed files needs a commit message. The client pushes the selected repositories and then tells the oracle to close the codespace. If a push fails, the codespace stays open.

In a codespace with more than one repository, the **Changes** panel shows the changes of each repository under its directory name.

The model picker shows the account name of each model provider.

The tooltip of a codespace shows when the oracle stops it after a time without activity, and when it deletes a stopped codespace. See [Codespace lifecycle](oracle.md#codespace-lifecycle).

When the host of a codespace has an SSH destination, the client connects to the agent through SSH first. If SSH fails, it uses the relay of the oracle. See [SSH connections](host.md#ssh-connections).

### Oracle settings

You do all configuration of an oracle in the desktop client. Click the gear button next to the oracle under **Workspaces**, or run **oracle: settings** in the command palette. The settings replace the panes until you open a session or click the close button. Unsaved changes stay when you close the settings.

| Page | Contents |
| --- | --- |
| Devices | The paired devices. **Add device…** shows a pairing link and a QR code. You can rename and revoke a device, and cancel an open link. |
| Hosts | The hosts with their status, their report, and the results of `loom-host check`. **Add host…** shows a join link. **Check** runs the check again, and **Check with Docker** also tests Docker. You can set the environment, the setup script, and the SSH destination of each host. **Limits…** sets the maximum codespaces, the maximum running codespaces, the idle stop, and the retention of a host. See [Codespace limits](oracle.md#codespace-limits). The page shows the volumes on each host with their sizes. |
| Git providers | The URLs, the Git name and email, and the route of each provider. The Git credentials of each provider with their scope, login, and expiry date. You can check a token and replace it. The page shows a warning for a token that expires in 14 days or less. |
| Models | The model accounts with their provider, kind, email, and organization. **Add account…** signs in with the browser or a device code, or adds an API key. The client catches the redirect of the browser when the provider uses a local URL. Otherwise, paste the code or the URL. |
| Secrets | The names of the secrets, and the presets and variables that use each secret. You can set, replace, and delete a value. You cannot read a value. |
| Presets | The presets and the credentials that each names. **Edit** opens `agent.yaml`, `AGENTS.md`, and the skill files in a text editor. **Import from this computer…** reads `agent.yaml`, `AGENTS.md`, and the skills in `~/.config/loom`, and removes the sections that a codespace sets itself. **Duplicate…** copies a preset with a new name. |
| Workspaces | The repositories, the volumes, and the host of each workspace, and the volumes of the oracle. **New workspace…** opens the repository search of a Git provider. When you select a repository, the name of the workspace comes from it. Add more repositories and volumes, select the host, and click **Create**. **Edit** changes the host, the repositories, and the volumes. |
| Audit log | The changes that devices, hosts, and the oracle made, newest first. **Show older entries** loads more. |

- The oracle validates each change when you save it. If it refuses the change, the form or a notification shows the reason.
- The local rules of a host are not in the client. They stay in the file on the host.
- The first device pairs with the link of `loom-oracle pair` on the oracle machine. After that, add the other devices on the **Devices** page.

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

The base is the upstream branch of the current branch, for example `origin/main`. If there is no upstream branch, the panel shows no commits.

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
