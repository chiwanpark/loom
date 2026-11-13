# Changelog

All notable changes to Loom are listed here, newest first. Versions follow [Semantic Versioning](https://semver.org). While Loom is on 0.x, a minor version may include breaking changes.

## Unreleased

- Each session has an operation thread, a text board for the agent's notes on the work that stays when the conversation is compacted or cleared. The agent uses it through the new `thread_read`, `thread_edit`, and `thread_write` tools, and you can read and edit it in the client's new **Thread** tab. `tools.thread.max_bytes` in `agent.yaml` limits its size (64 KiB by default). See [Operation thread](README.md#operation-thread).
- The protocol version is now 7. Agents and clients of 0.1.0 (protocol 6) cannot connect to these, so upgrade both. Running agents keep their old version until you restart them.
- New keys for tabs, which also work in terminals: `ctrl-tab` / `ctrl-shift-tab` for the next / previous tab, `alt-1` to `alt-8` for that tab and `alt-9` for the last one, and `ctrl-shift-pagedown` / `ctrl-shift-pageup` to move a tab right / left. `ctrl-shift-b` shows the Agents panel and `ctrl-shift-t` the Thread panel.

## 0.1.0

The first release.
