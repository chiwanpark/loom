# Development

## Crates

| Crate | Contents |
| --- | --- |
| `crates/loom-agent` | The agent `loom-agent` of a codespace and its commands. |
| `crates/loom-gui` | The desktop client `loom`, built with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui). |
| `crates/loom-relay` | The relay that the oracle runs. |
| `crates/loom-oracle` | The oracle `loom-oracle`: the relay, the control service, and the store. See [Oracle](oracle.md). |
| `loom-oracle-proto` | The JSON messages of the control service. |
| `crates/loom-host` | The host daemon `loom-host`. It starts and deletes the codespaces of the oracle. See [Hosts and codespaces](host.md). |
| `loom-incus` | A client for the REST API of Incus, over the local socket or HTTPS with a client certificate. The feature `fake` adds a fake Incus server for tests. |
| `loom-agent-core` | The agent core. |
| `loom-llm` | The model providers. |
| `loom-tools` | The tools and the sandbox. |
| `loom-proto` | The wire protocol. |
| `loom-transport` | The transports. |
| `loom-client` | The client library. Its default feature `cli` adds the test client `loom-oracle-cli` for the control service of the oracle. The desktop and Android clients do not use this feature. |
| `loom-view-model` | The view model that the clients share. |
| `loom-mobile` | The Rust core of the Android client and its Kotlin bindings. |
| `loom-config` | The configuration. |
| `loom-crypto` | The key management. |
| `loom-vt` | The terminal emulation. |
| `loom-telemetry` | The OpenTelemetry export. |

The Android app is in `android/`.

## Checks

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check
```

To build from source, see [Build from source](install.md#build-from-source). To build the Android app, see [Android app](android.md#build).
