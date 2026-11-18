# Android app

The Android app connects to agents through a relay only. It needs Android 17 (API 37) or newer on arm64.

## Build

Releases do not include the app yet. You must build it from source.

CI builds a debug APK for each change. It is the `loom-android-debug` artifact of a CI run. Each run signs the APK with a new debug key. Thus, you cannot install one of these APKs over a different one.

You need these items:

- JDK 21.
- The Android SDK with platform 37, build tools 37.0.0, and NDK 30.0.16248370.
- The Rust target `aarch64-linux-android`.
- [cargo-ndk](https://github.com/bbqsrc/cargo-ndk).

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_HOME=~/Android/Sdk
cd android && ./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Gradle builds the Rust core (`crates/loom-mobile`) with cargo-ndk. It makes the Kotlin bindings with UniFFI. Thus, the app always uses the same protocol as the agents from the same source tree.

### Signing key

Android replaces an installed app only with an APK that has the same signing key. To sign all builds with your own key, also debug builds, put the key details in `~/.config/loom-android/keystore.properties`. To use a different file, set `LOOM_ANDROID_KEYSTORE_PROPERTIES` or `-Ploom.keystoreProperties`.

```properties
storeFile=/home/me/.config/loom-android/release.jks
storePassword=…
keyAlias=loom
keyPassword=…
```

Without this file, debug builds use the debug key of the SDK.

Keep the key and its password safe. An APK with a different key installs only after you uninstall the app. This deletes the pairings of the app.

### Notifications

Notifications need a Firebase project.

1. In the [Firebase console](https://console.firebase.google.com), make a project.
2. Add an Android app with the package name `com.chiwanpark.loom`.
3. Put its `google-services.json` in `~/.config/loom-android/`. To use a different file, set `LOOM_ANDROID_GOOGLE_SERVICES` or `-Ploom.googleServices`.

Builds without this file, such as the CI builds, work without notifications.

The relay also needs a key of the same Firebase project. See [Notifications for the Android app](remote-hosts.md#notifications-for-the-android-app).

## Pair with a host

1. Set up a relay and the agent. See [Relays](remote-hosts.md#relays).
2. On the agent host, run `loom-agent pair`.
3. In the app, use **Pair with a host**. Scan the QR code or paste the link.

To scan the QR code, the phone needs Google Play services. To paste the link, it does not.

## Sessions

The start screen shows all sessions of all agents on the paired hosts. It puts them into groups by state, as the Sessions panel of the desktop client does. On this screen, you can start, rename, archive, and delete sessions. A completed session becomes idle when you open it in the app.

A session in a worktree shows its branch after its title, as in the Sessions panel of the desktop client.

In a session, you can do these steps:

- Read the conversation.
- Open tool calls to see their arguments and output.
- Approve or deny tool calls.
- Send messages, and queue or steer a running turn with them. See [Messages during a turn](sessions.md#messages-during-a-turn).
- Stop a turn.
- Compact or clear the conversation.
- Rewind the conversation to one of your messages. Long-press the message. See [Rewind a conversation](sessions.md#rewind-a-conversation).
- Edit the notes of the agent and the tasks of the session.

The row above the prompt box shows the context used. It also lets you select these settings:

- The model.
- The reasoning effort.
- The action for tool calls that no policy rule matches: ask, allow, allow harmful, deny, or the default of the agent. See [Policy and harmful tool calls](policy.md).
- The [comment guard](comment-guard.md).

### Start a session

1. Press **+** on the start screen.
2. Select a connected agent, or select **In another directory…** on one of their hosts.
3. For a different directory, type the directory, or select a directory from the list. `~` is your home directory on the host. The list has directories where you started sessions before, and directories of running agents.

The agent of the host starts an agent in the directory, or uses the agent that already runs there. The session opens when the app connects to this agent through the relay. The directory must be in one of the `allowed_roots` of the agent configuration of the host.

To start a session in a worktree, press the branch button next to the agent. See [Sessions in a worktree](sessions.md#sessions-in-a-worktree).

### Host colors

You can give each host a color. The color helps you to identify the sessions of each host.

1. In the menu of the start screen, open **Hosts**.
2. In the menu of the host, choose **Color…**.

If you have more than one host, each row of the start screen then shows the name of the host in this color.

## Changes and files

In the menu of a session:

- **Changes** shows the changes in the workspace of the agent, as the Changes panel of the desktop client does.
  - The first tab shows the uncommitted changes in git. A robot mark shows the files that the session also changed.
  - The second tab shows the files that the session changed with its tool calls.
  - Tap a file to open its diff, with the line numbers of the old file and the new file.
- **Files** shows the workspace of the agent.
  - Directories come first. Files that git ignores are dimmed.
  - Tap a file to open it with line numbers. The file viewer is read-only. It shows a maximum of the first 2 MiB of a file.

Images (PNG, JPEG, WebP, GIF, BMP, HEIF, and AVIF) and PDF files up to 64 MiB open in a viewer. The viewer copies the file to the cache of the app while the file is open.

- Pinch to zoom.
- Move a zoomed image with a finger.
- Double-tap an image to zoom in or out.
- You can also scroll a zoomed PDF file to the side.

Files and diffs have syntax highlights from the grammars of [bat](https://github.com/sharkdp/bat). The wrap button of the file viewer and the diff viewer wraps long lines, or lets you scroll them to the side.

## Terminal

**Terminal** in the menu of a session opens a terminal on the agent host. The terminal runs your shell in the workspace of the agent.

A row above the keyboard has the keys that a phone keyboard does not have: Esc, Tab, arrows, Home, End, Page Up, Page Down, some symbols, Ctrl, and Alt. Ctrl and Alt apply to the next key. A hardware keyboard also works.

- Drag to scroll back.
- Pinch to change the text size.
- Use the menu to paste, copy the screen, change to a different terminal, or open a new terminal.

The terminal changes its size when the keyboard opens or closes and when the phone turns.

The terminal continues to run when the connection stops. The app gets it back when it connects again. These conditions are exceptions:

- The agent closed the terminal after `terminal.detached_timeout`.
- A different client took the terminal.

The menu also shows the terminals that are open on other clients, such as the desktop client. To move a terminal to the phone with its recent output, select it and press **Take over**.

## Ports

**Ports** in the menu of a session shows the TCP ports on the agent host where programs of the agent user listen. An example is a development server.

To forward a port of the phone to a port of the agent host, press **Forward** on a port, or type a port or `host:port`.

- The forward listens on the loopback address of the phone. It uses the same port number if it is free. Thus, the browser and other apps of the phone can open the server at `localhost`.
- Forwards continue when Loom is not on the screen. A notification shows the forwards and lets you stop them.
- When the app connects again or follows a restarted agent, it starts the forwards again on the same ports.

## Connections

The app keeps its connections to the agents while it runs. If a connection stops, the app connects again automatically. Open conversations then get the messages that they did not get.

When the app comes back on the screen or the phone changes networks, the app examines each connection immediately. It does not wait for the heartbeat timeout.

A restarted agent runs as a new instance. The app finds it by its workspace on the relay of the host. Open screens follow the new instance.

If the app and an agent use different protocol versions, the app tells you which of the two to update.

To update the agents of a host, use **Update agents…** in the menu of the host. See [Agents of a host](install.md#agents-of-a-host).

## Notifications

If you build the app with a Firebase project, it shows a notification in these conditions, also when the app is closed:

- A tool call waits for approval.
- A turn is complete or fails.

Tap a notification to open the session. The app does not show notifications for the session on the screen. Each type of notification has its own channel in the notification settings of the system.

When the app connects to a paired host, it tells the host where to send notifications. The agents of the host encrypt each notification with the keys from the pairing. Then they send it to their relay, which sends it through Firebase Cloud Messaging. Thus, Google sees only encrypted data.

## Fonts

**Font** in the menu of the start screen or of a session opens a font screen with a preview. You can set these items:

- The text size, from 80% to 130% of the system size.
- The size of the text in blocks, from 70% to 130% of the app text. Blocks are the arguments and output of tool calls, and the reasoning of answers.
- The font of the app text.
- The font of code. Code is the arguments and output of tools, code in answers, notes, and keys.

The default code font is JetBrains Mono, which is part of the app.

In addition to the fonts of the system, you can search all Google Fonts by name:

- You can filter by script, such as Korean, Japanese, or Chinese.
- For text, you can also filter by category.
- The code tab shows only monospaced fonts.

Each of the two fonts can also have a fallback font from Google Fonts. The fallback font shows the characters that the font does not have, such as Hangul in JetBrains Mono. The app uses the fallback font before the fonts of the system.

Google Play services downloads a Google font when the app shows it for the first time.

The list of Google Fonts of the app is `android/app/src/main/assets/google-fonts.tsv`. `scripts/fonts/generate.py` writes this list from the directory of the Google Play services font provider and the metadata of fonts.google.com. To add new fonts to the list, run the script again.
