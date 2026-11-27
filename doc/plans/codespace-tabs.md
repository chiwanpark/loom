# Plan: codespaces panel, session tabs, and archived sessions

Status: planned, not started.

## Goals

- Make the hierarchy in the clients simpler: oracle → codespace → tabs.
- Rename the **Sessions** panel to **Codespaces**. The panel lists only codespaces.
- Show sessions as tabs in the main panel, at the same level as terminals, diffs, and files. A session and its conversation are the same thing for the user.
- Do the same changes in the desktop client and in the Android app.

## New session lifecycle

- Users cannot delete sessions. Remove all "Delete" actions from the clients.
- Closing a session tab archives the session on the agent.
- When a session is archived while it is empty, the agent deletes it.
- Open sessions = sessions that are not archived = session tabs. When a client opens a codespace, it shows all sessions that are not archived as tabs.
- Closing a session tab archives the session on the agent, so the tab also closes in other clients.
- Remove all lists of archived sessions, because they become very long:
  - the "Archived" group of the desktop panel;
  - the archived items of the desktop **Find session** picker;
  - "Show archived sessions" on the Android home screen.
- To resume an archived session, the user clicks a new **Resume** button next to the **Clear** button. A dialog shows the archived sessions of the workspace. The selected session opens as a tab.

## Archived sessions belong to the workspace

- The oracle keeps its session archive (`codespaces.archive`, on by default, kept for 90 days). When a codespace closes, the oracle copies the session store of its agent to `archive/<codespace>/`.
- A session from a closed codespace can be resumed in another codespace of the same workspace. The repositories have the same layout under `/workspaces`, so the paths in the history stay correct.
- The Resume dialog lists:
  - the archived sessions of the agent of this codespace;
  - the sessions in the archives of the closed codespaces of the same workspace.
- The dialog does not list archived sessions of other live codespaces. Moving a session between two running agents is a separate feature.
- A closed codespace is gone from the clients. Remove the client UI for the archives of closed codespaces.

## Work items

### 1. Agent core (`crates/loom-agent-core`)

- `Agent::set_archived(id, true)` deletes the session when its history is empty and no messages wait. Then it sends `AgentNotice::SessionRemoved`.
- Add an import of a session from an archived store: state (`SessionState`), task list, and events. The session keeps its ID unless the agent already has that ID.
- Tests in `tests/session.rs` and `tests/store.rs`.

### 2. Oracle (`crates/loom-oracle`, `crates/loom-oracle-proto`)

- Add calls for the agent of a codespace. The oracle permits only the workspace of the caller's codespace:
  - list the archived sessions of the workspace: the sessions in the archives of its closed codespaces, without the empty ones and the ones that were resumed;
  - fetch one archived session: its state and task list, and its events in pages (as `archive_events` does now);
  - mark an archived session as resumed, so that it leaves the list.
- Keep `archive_before_delete`, the retention, and the host calls `ArchiveFiles` and `ArchiveChunk`.
- Remove the client calls that only the archive viewer uses, if no other client uses them.
- Tests in the oracle crate.

### 3. Agent protocol and agent (`crates/loom-proto`, `crates/loom-agent`, `crates/loom-client`)

- `Command::ListArchivedSessions`: the archived sessions of this agent, and the archived sessions of the workspace from the oracle. Each item has an ID, a title, times, a model, and where it is (this agent, or a closed codespace with its branch).
- `Command::ResumeSession { session, codespace: Option<String> }`:
  - without `codespace`: unarchive the session of this agent;
  - with `codespace`: the agent fetches the session from the oracle, imports it, tells the oracle that it was resumed, and answers with the new `SessionInfo`.
- Keep `Command::DeleteSession` for compatibility, but the clients do not use it.
- Add the calls to `loom-client`.
- Change `PROTOCOL_VERSION` if the new commands need it.

### 4. Desktop: tab groups per codespace (`crates/loom-gui`)

- Replace `SessionGroup { connection, session, … }` in `ui/workspace.rs` with one group per codespace connection.
- A group can have many `Panel::Chat` tabs, one for each session that is not archived.
- `Panel::closable` returns true for session tabs. Closing a session tab archives the session (`archive_session`).
- The group keeps its current session: the session of the active session tab, or the session tab that was active last. These use the current session:
  - the Tasks tab;
  - session changes and session diffs;
  - `mark_seen`;
  - the system prompt;
  - Rename and Archive.
- When the agent lists a session that is not archived and has no tab, add a tab without focus. When a session is archived or removed, close its tab.
- Terminals of other clients go to the group of their codespace (`sync_terminal_tabs`).
- A session tab shows the state of its session (needs approval, working, completed) with an icon. Rename and Archive are in the tab's context menu.
- Add a "+" menu to the tab bar with "New session" and "New terminal".
- `layout.rs`: version 2. Each group is stored by host and workspace path, without a session. `SavedTab::Conversation` gets the session ID. The client does not convert a version 1 file; it starts with an empty layout once.

### 5. Desktop: Codespaces panel

- Rename `RightTab::Sessions` to `RightTab::Codespaces`, with the title "Codespaces". Rename the actions `ToggleSessions` and `ShowSessions`, and keep the old names as aliases in the keymap.
- The panel shows one flat list of the codespaces of all oracles. Each row shows:
  - `workspace · branch`;
  - `oracle · preset · state · N sessions`, with the oracle name in the host color;
  - the combined state of its sessions (needs approval, working, completed), or the codespace state.
- Click a codespace to open its tab group. The row menu keeps New session, Open pull request, Show start progress, Stop, Resume codespace, and Close.
- The panel header has a "+" button that opens the workspace picker (`workspace_picker`) for a new codespace.
- An "Oracles" section at the bottom of the panel has, for each oracle: its status, Refresh, the host color, Settings, and Remove.
- Remove `render_session_row`, `render_group_header`, the use of `agent_list::group`, `ui/workspace/archives.rs`, `ui/archive_view.rs`, and the "Delete…" items.
- **Find session** lists only sessions that are not archived.

### 6. Desktop: Resume dialog

- Add a **Resume** button next to the **Clear** button in `ui/session.rs`.
- The dialog lists the result of `ListArchivedSessions`, newest first, with a search field. Each item shows its title, its time, and where it is.
- Selecting an item calls `ResumeSession` and opens the session as a tab in the same codespace group.

### 7. Android (`android/`, `crates/loom-mobile`)

- The home screen lists codespaces, with the same rows as the desktop panel. The Oracles section and "+ New codespace" are in the menu.
- A new codespace screen has a tab row at the top that you can scroll. It holds the open sessions and the terminals. The content below is the conversation or the terminal.
- "+" in the tab row offers "New session" and "New terminal".
- Closing a session tab archives the session.
- Files, Changes, Notes, Tasks, and Ports stay in the menu.
- Add "Resume a session" next to "Clear the conversation" in the conversation menu. It opens the same dialog as the desktop client.
- Remove Delete, Archive/Unarchive, and the archived list from `HomeScreen.kt`.
- Add the new calls to `loom-mobile` and its Kotlin bindings.

### 8. Documentation and checks

- Update `doc/sessions.md`, `doc/desktop.md`, `doc/android.md`, and `doc/oracle.md` (Session archive).
- Add entries to `## Unreleased` in `CHANGELOG.md`.
- Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and the Android build.

## Build environment

The work needs:

- Rust stable with clippy and rustfmt;
- `build-essential`, `pkg-config`, `libfontconfig-dev`, `libfreetype-dev`, `libxkbcommon-dev`, and `libwayland-dev`, as in `.github/workflows/ci.yaml`;
- for the Android app: a JDK, the Android SDK, and an NDK (see `doc/android.md`);
- more than 4 GB of RAM to build `loom-gui` and the Android app.
