# Hosts and codespaces (Loom-Next)

A host runs the codespaces of the oracle. The daemon `loom-host` connects out to the oracle and starts and deletes the codespaces of the host. The plan is in [the design document](design/loom-next.md). This page describes the state after phase 2.

In phase 2, a workspace has one public repository. A codespace clones it, makes the branch `loom/<codespace>`, and starts the agent. The current desktop client connects to the agent through the relay of the oracle.

The releases do not contain `loom-host` yet. Build it from the branch `loom-next`. The host also needs a build of `loom-agent` from this branch, because the managed mode is new:

```sh
cargo build --release --bin loom-host --bin loom-agent
```

## Drivers

| Driver | Use |
| --- | --- |
| `process` | Starts each codespace agent as a local process of the user of `loom-host`. There is no container and no limit. Use it for tests and for the development of Loom. |

## Set up a host

1. On a device of the oracle, run `loom-oracle-cli pair-host`. It shows a one-time link.
2. On the host, run `loom-host join '<link>' --name <name>`. The default name is the host name. The host keeps the address of the oracle and its key in the data directory.
3. Run `loom-host config init`, and set the driver and the other keys in `~/.config/loom/host.yaml`.
4. Run `loom-host check`. It shows the oracle, the agent binary, and the results of the driver.
5. Run `loom-host serve`. It connects to the oracle and reports the CPUs, the memory, and the instances of the host. If the connection stops, it connects again.

`loom-oracle-cli hosts` then shows the host as `connected`.

If the oracle is behind Cloudflare Access, join with `--access-id-env <name> --access-secret-env <name>`. These are the names of the environment variables that contain the service token.

## `host.yaml`

| Key | Meaning |
| --- | --- |
| `driver` | `process`. |
| `data_dir` | The directory of the key and the address of the oracle. The default is `~/.local/share/loom/host`. |
| `process.root` | The directory of the codespaces of the `process` driver. The default is `codespaces` in `data_dir`. |
| `agent.path` | The `loom-agent` binary for the codespaces. The default is `loom-agent` in the directory of `loom-host`. |
| `rules.max_codespaces` | The maximum number of codespaces on this host. The oracle cannot change this rule. |
| `report_interval` | How often the host sends its instances to the oracle. The default is `60s`. |
| `logging.level` | The log level. |

`loom-host config check` validates the file, and `loom-host config schema` shows its JSON Schema. `--config` or `LOOM_HOST_CONFIG` selects a different file.

## Make a codespace

With `loom-oracle-cli`:

```sh
loom-oracle-cli create-workspace acme --host <host> --repository https://github.com/acme/app.git
loom-oracle-cli create-codespace acme --cpus 2 --memory 4GiB
```

`create-codespace` shows the steps until the codespace is ready. Then it shows a host entry for `client.yaml` of the desktop client. `loom-oracle-cli client-entry <id>` shows the entry again.

The device keys of the oracle are the keys that the agent of a codespace accepts. Thus, the desktop client must use a key that is paired with the oracle. Pair the key of the desktop client with `loom-oracle-cli pair <link> --identity ~/.local/share/loom/client_identity.json`.

`loom-oracle-cli close-codespace <id>` deletes the instance of the codespace and revokes the keys of its agent.

## Codespace agents

In a codespace, `loom-agent serve --managed` runs the agent:

1. The host puts a bootstrap file into `~/.config/loom/bootstrap.json` of the agent. The file contains a one-time link for this codespace.
2. The agent makes its keys and pairs with the oracle. Then it deletes the bootstrap file. It keeps the address of the oracle in `managed.json` in its data directory, so it can start again without the link.
3. The agent clones the repository into the workspaces root and makes the branch `loom/<codespace>`.
4. The agent accepts the keys of the devices of the oracle. When you pair or revoke a device, the agent gets the change.
5. The agent registers with the relay of the oracle and reports `ready`.

`--bootstrap <path>` selects a different bootstrap file.

## Codespace states

The oracle compares the instances that the host reports with its codespaces:

- It deletes an instance that has no open codespace.
- A codespace whose instance is missing for more than 60 seconds becomes `lost`.
- If the host was not connected when you closed a codespace, the oracle deletes the instance after the host connects again.
