# Development

## Crates

| Crate | Contents |
| --- | --- |
| `crates/loom-agent` | The agent daemon `loom-agent` and its commands. |
| `crates/loom-gui` | The desktop client `loom`, built with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui). |
| `crates/loom-relay` | The relay `loom-relay`. |
| `loom-agent-core` | The agent core. |
| `loom-llm` | The model providers. |
| `loom-tools` | The tools and the sandbox. |
| `loom-proto` | The wire protocol. |
| `loom-transport` | The transports. |
| `loom-client` | The client library. |
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
