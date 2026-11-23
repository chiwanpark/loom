# Oracle (Loom-Next)

The oracle is the central service of Loom-Next. The plan is in [the design document](design/loom-next.md). This page describes the state after phase 3.

The oracle does these things:

- It is a relay. Current agents and clients use it as they use `loom-relay`.
- It has a control service. Paired devices manage hosts, Git providers and credentials, presets, workspaces, and volumes through this service.
- It starts codespaces on hosts. See [Hosts and codespaces](host.md).
- It keeps its data in a SQLite store. It encrypts the secrets in the store with a master key.

The releases do not contain the oracle yet. Get `loom-oracle` from the `loom-next` CI archives or build it from the branch.
Build the test client from source:

```sh
cargo build --release --bin loom-oracle --bin loom-oracle-cli
```

## Components

| Binary | Use |
| --- | --- |
| `loom-oracle` | The daemon. It has three commands: `serve`, `config`, and `pair`. |
| `loom-oracle-cli` | A test client for the control service. Use it until the desktop client has the oracle pages. |

## Set up an oracle

1. Run `loom-oracle config init`. It writes `~/.config/loom/oracle.yaml`.
2. In `oracle.yaml`, set `url` to the URL that clients and agents use, for example `wss://oracle.example.com`.
3. Run `loom-oracle serve`. At the first start, the oracle makes its master key, its host keys, and its store.
4. Run `loom-oracle pair` on the same machine. It shows a one-time `loom://` link and a QR code for the first device.
5. On the device, run `loom-oracle-cli pair <link>`. `loom-oracle pair` stops when the device is paired.

After this, the first device makes the links for the other devices with `loom-oracle-cli pair-device`.

`loom-oracle config check` validates `oracle.yaml`. `loom-oracle config schema` shows its JSON Schema. `--config` or `LOOM_ORACLE_CONFIG` selects a different file.

## `oracle.yaml`

The keys are the same as in `relay.yaml`, with these differences:

| Key | Meaning |
| --- | --- |
| `url` | The public URL of the oracle. Pairing links contain it. If it is not set, the oracle uses `ws://` or `wss://` with `listen`. |
| `data_dir` | The directory of the store and the host keys. The default is `~/.local/share/loom/oracle`. |
| `master_key` | The file of the master key. The default is `master.key` in `data_dir`. |
| `allowed_hosts` | Not available. Use relay hosts. See [Use the oracle as a relay](#use-the-oracle-as-a-relay). |

These keys are the same as in `relay.yaml`: `listen`, `metrics_listen`, `tls`, `cloudflare_access`, `push`, `limits`, and `logging`.

## Master key

The master key encrypts the pairing secrets and Git tokens in the store.

- At the first start, the oracle makes the key file with mode `0600`.
- Keep a copy of the key in a safe place, apart from the store.
- The oracle does not start if the key is missing or if the key does not match the store.

The key can also be a systemd credential. If `master_key` is not set and `$CREDENTIALS_DIRECTORY/master.key` exists, the oracle uses this file. For example, add this line to the service with `sudo systemctl edit loom-oracle.service`:

```ini
[Service]
LoadCredential=master.key:/etc/loom/oracle-master.key
```

`loom-oracle pair` also needs the key. If the key is a credential, give the same credential to the `pair` command.

## Run the oracle as a system service

`contrib/systemd/loom-oracle.service` runs the oracle as the system user `loom-oracle`. The data directory is `/var/lib/loom-oracle`.

```sh
sudo useradd --system --home-dir /var/lib/loom-oracle --shell /usr/sbin/nologin loom-oracle
sudo install -m755 loom-oracle /usr/local/bin/
sudo loom-oracle --config /etc/loom/oracle.yaml config init
```

In `/etc/loom/oracle.yaml`, set `url` and add `data_dir: /var/lib/loom-oracle`. Then start the service and pair the first device:

```sh
sudo install -m644 contrib/systemd/loom-oracle.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now loom-oracle.service
sudo -u loom-oracle loom-oracle --config /etc/loom/oracle.yaml pair
```

## Test client

`loom-oracle-cli` keeps its key and the address of the oracle in `~/.local/share/loom/oracle-cli`. `--state-dir` or `LOOM_ORACLE_CLI_STATE` selects a different directory. Thus, one machine can act as more than one device.

| Command | Action |
| --- | --- |
| `pair <link> [--label <label>] [--identity <path>]` | Pairs with the oracle. The default label is the host name. `--identity` uses the key in this file, for example the key of the desktop client. |
| `info` | Shows the oracle and this device. |
| `devices` | Lists the paired devices. |
| `rename-device <id> <label>` | Changes the label of a device. |
| `revoke-device <id>` | Removes a device. Its connections close at once. |
| `push-token <token>` or `push-token --clear` | Sets or removes the push token of this device. |
| `pair-device [--ttl <duration>] [--no-qr]` | Makes a one-time link for a new device. The default time is 10 minutes. |
| `pairings` | Lists the links that are not used yet. |
| `cancel-pairing <id>` | Cancels a link. |
| `relay-hosts` | Lists the relay hosts and their connected agents. |
| `add-relay-host <host_id> <label>` | Lets the agents of a host register. |
| `remove-relay-host <host_id>` | Removes a relay host. Its agents disconnect at once. |
| `pair-host [--ttl <duration>] [--no-qr]` | Makes a one-time link for `loom-host join`. |
| `hosts` | Lists the hosts with their status and their report. |
| `remove-host <name>` | Removes a host. The oracle refuses this while it has workspaces, open codespaces, or tracked volumes. |
| `set-environment <host> [--base-image <image>] [--setup-script <path>] [--env <name>=<value>]… [--ca-certificate <path>]… [--registry-mirror <url>]…` | Replaces the environment configuration of a host. The host builds a new image for the next codespace. |
| `providers` | Lists the Git providers. |
| `set-provider <name> --url <url> [--api-url <url>] [--git-name <name>] [--git-email <email>] [--route direct\|hosts]` | Adds or changes a Git provider. The default API URL is `https://api.github.com` for `https://github.com`, and `<url>/api/v3` for other servers. |
| `delete-provider <name>` | Deletes a Git provider. The oracle refuses this while a credential or a workspace uses it. |
| `credentials` | Lists the Git credentials. The list does not show the tokens. |
| `set-credential <name> --provider <provider> [--owner <owner> [--repository <repository>]] [--token-file <path>]` | Adds or changes a Git credential. `--token-file -` reads the token from standard input. A new credential needs a token. The oracle checks the token. |
| `check-credential <name>` | Checks the token again. |
| `delete-credential <name>` | Deletes a Git credential. The oracle refuses this while a preset uses it. |
| `presets` | Lists the presets. |
| `set-preset <name> <path>` | Adds or changes a preset from an `agent.yaml` file. |
| `delete-preset <name>` | Deletes a preset. The oracle refuses this while an open codespace uses it. |
| `search <provider> [<query>]` | Searches the repositories of a provider. |
| `branches <provider> <owner>/<repository>` | Lists the branches of a repository. |
| `workspaces` | Lists the workspaces. |
| `create-workspace <name> --host <host> --repository <repository>… [--volume <mount>]…` | Makes a workspace. Give each repository as `<url>[,directory=<name>][,branch=<branch>]`. `<url>` can also be `<provider>:<owner>/<repository>`. |
| `update-workspace <name> [--host <host>] [--repository <repository>…] [--volume <mount>… \| --no-volumes]` | Changes the host or replaces the repositories or mounts. Open codespaces do not change. |
| `delete-workspace <name>` | Deletes a workspace. The oracle refuses this while a codespace of the workspace is open. |
| `volumes` | Lists the volumes and the hosts that may hold each custom volume. |
| `create-volume <name> [--size <size> \| --path <host-directory>]` | Defines a volume. Without `--path`, it defines a custom Incus volume. |
| `delete-volume <name>` | Deletes the definition and its custom volume on each tracked host. It does not delete the contents of a host directory. |
| `codespaces [--all]` | Lists the codespaces. `--all` also shows the closed codespaces. |
| `create-codespace <workspace> [--preset <preset>] [--cpus <n>] [--memory <size>] [--branch <branch>] [--base <directory>=<branch>]… [--no-wait]` | Makes a codespace and shows its steps until it is ready. The defaults are 2 CPUs and 4GiB. `--branch` sets the base branch of all repositories, and `--base` sets it for one repository. |
| `close-codespace <id> [--push <directory>]… [--discard <directory>]… [--message <message>] [--force] [--no-wait]` | Closes a codespace. The host deletes its instance. First, the close guard asks the agent for the unsaved work of each repository. See [the host document](host.md#git-in-a-codespace). |
| `repositories <id>` | Shows the state of each repository of a codespace. |
| `push <id> [--message <message>] [--directory <directory>]…` | Pushes the branch of each repository of a codespace. With `--message`, the agent first commits the uncommitted changes. |
| `pull-request <id> --title <title> [--body <text> \| --body-file <path>] [--draft]` | Opens a pull request for each repository of a codespace that has commits. |
| `client-entry <id> [--name <name>]` | Shows the host entry for `client.yaml` of the desktop client. |
| `watch [--count <n>]` | Shows the events of the oracle until you stop it. |

With `--json`, each command writes JSON.

If the oracle is behind Cloudflare Access, pair with `--access-id-env <name> --access-secret-env <name>`. These are the names of the environment variables that contain the service token. The CLI keeps the names, not the values.

## Use the oracle as a relay

Current agents and clients use the oracle as their relay without a change.

1. On the agent host, run `loom-agent host`. It shows the `host_id` of the host.
2. Run `loom-oracle-cli add-relay-host <host_id> <label>`. The oracle does not need a restart.
3. In `agent.yaml` of the agent, set `relay.enabled: true` and set `relay.url` to the URL of the oracle.
4. Pair the clients with `loom-agent pair`, as with `loom-relay`.

## Limits

The oracle and `loom-relay` use the same limits from `limits` in their files. A connection uses the limits of its IP address until it proves its identity:

- An agent uses its own limits after it registers.
- A device uses its own limits after it opens a channel to the control service.

Thus, many agents and devices behind one public IP address do not share one limit. Before this change, all limits applied to each IP address.

## Git providers

A Git provider is github.com or a GitHub Enterprise Server. It has a name, a web URL, an API URL, an optional Git identity, and a route.

- With the route `direct`, the oracle sends API calls to the provider.
- With the route `hosts`, the oracle sends each API call through a connected host that reaches the provider. Use this route when the oracle cannot reach the provider.

Each host tests which providers it reaches when it connects and when a provider changes. `loom-oracle-cli hosts` shows the result. The oracle keeps a result for 10 minutes.

A workspace can have up to 20 repositories. The oracle finds the provider of a repository from the start of its URL. A repository whose URL matches no provider is public: the codespace clones it without a credential.

The host of a workspace must reach the providers of all its repositories:

- When you save a workspace, the oracle refuses it if the host is connected and cannot reach a provider.
- When you make a codespace, the oracle refuses it if the host is not connected or cannot reach a provider.

### Git credentials

A Git credential is a token of a provider, for example a fine-grained personal access token. The oracle keeps the token encrypted with the master key. A client cannot read a token back.

A credential has a scope:

- all repositories of the provider;
- the repositories of one owner (`--owner`);
- one repository (`--owner` and `--repository`).

When the oracle saves a credential, it reads the user and the expiry date of the token from the provider.

### Presets

A preset is an `agent.yaml` file. The oracle validates it when you save it. Phase 3 uses only its `git.credentials` section, a list of credential names:

```yaml
git:
  credentials: [ghe-work, github-personal]
```

A codespace with a preset can use these credentials. For each repository, the codespace uses the credential with the smallest scope that matches. If two credentials have the same scope, the first in the list wins.

The codespace configures Git for each provider:

- The Git credential helper asks the oracle for the token of each repository when Git needs it. Git does not keep the token.
- `gh` uses the credential of the first repository of the provider. The codespace writes this token to the configuration file of `gh`.
- The Git name and email come from the provider. If the provider has no name, the codespace uses the login of the credential. If the provider is github.com and has no email, the codespace uses the noreply address of the login. For other providers, set the email on the provider. When the codespace has no email for a provider, the oracle shows a warning.

When a provider, a credential, or a preset changes, the oracle sends `git_changed` to the codespaces. They read the new setup.

## Workspace volumes

A volume keeps data after a codespace closes. Define the volume first, then give it a mount path in each workspace that uses it:

```sh
loom-oracle-cli create-volume build-cache --size 4GiB
loom-oracle-cli create-volume reference-data --path /srv/codespace-data/reference
loom-oracle-cli create-workspace acme --host build-host --repository github:acme/app \
  --volume build-cache:/cache/build --volume reference-data:/data:ro
```

Use `VOLUME:PATH` for a writable mount or `VOLUME:PATH:ro` for a read-only mount.
Mount paths must be absolute and cannot overlap each other, the repository clones, the agent data directories, or system directories.
Host directory volumes must use paths permitted by the host. See [host setup](host.md#host-directories).
The oracle checks volume support and directory paths when it saves a workspace with a connected host. It checks again before it starts each codespace.

Updating a workspace replaces its mount list for new codespaces only. Use `--no-volumes` to remove all mounts from the workspace.
Before you delete a volume, remove its mounts from all workspaces and close the codespaces that still use it.
The oracle must reach each host where it requested the custom volume. If deletion is pending, fix the host error and run `delete-volume` again.
Hosts that already confirmed deletion are skipped on retry. A volume whose deletion is pending cannot be mounted again.

## Control service

The control service is an agent with the instance name `oracle` on the relay of the oracle. Clients connect to it as they connect to an agent: through the relay, with the Noise handshake, and with Cloudflare Access if it is set. A pairing link to the oracle has the instance name `oracle`.

Each paired peer has a role: `device`, `host`, or `codespace`. Each role has its own commands. All roles can call `oracle_info`.

- A device pairs with a link from `loom-oracle pair` or `create_device_pairing`.
- A host pairs with a link from `create_host_pairing`. If a host with the same label pairs again, it gets the new key and keeps its workspaces.
- A codespace agent pairs with the link in its bootstrap file. The link works only for its codespace, and the label of the peer is the ID of the codespace.

The messages are JSON. Receivers ignore fields and events that they do not know. The client and the oracle do not need the same version.

1. The client sends `{"type":"hello","version":1,"client":"<name>"}`.
2. The oracle replies with `hello`. The reply contains the oracle, the protocol version, and the peer: its ID, role, and label.
3. The client sends requests: `{"type":"request","id":<n>,"command":{"type":"<command>", …}}`.
4. The oracle replies with `{"type":"response","id":<n>,"result":{…}}` or with `"error":{"code":"…","message":"…"}`.
5. After `subscribe`, the oracle also sends `{"type":"event","event":{…}}`.
6. The oracle sends calls to a host: `{"type":"call","id":<n>,"call":{"type":"<call>", …}}`. The host replies with `{"type":"reply","id":<n>,"result":{…}}` or with `"error"`.

Commands of devices:

| Command | Fields | Result |
| --- | --- | --- |
| `oracle_info` | | `version`, `protocol`, `url`, `host_id` |
| `subscribe`, `unsubscribe` | | |
| `list_devices` | | `devices` |
| `rename_device` | `id`, `label` | `device` |
| `revoke_device` | `id` | |
| `set_push_token` | `token`, or no token to remove it | |
| `create_device_pairing` | `ttl_secs` (optional, 1 to 604800) | `pairing`, `link` |
| `list_pairings` | | `pairings` |
| `cancel_pairing` | `id` | |
| `list_relay_hosts` | | `relay_hosts` |
| `add_relay_host` | `host_id`, `label` | `relay_host` |
| `remove_relay_host` | `host_id` | |
| `create_host_pairing` | `ttl_secs` (optional) | `pairing`, `link` |
| `list_hosts` | | `hosts` |
| `remove_host` | `name` | |
| `set_host_environment` | `name`, `environment` | `host` |
| `list_git_providers` | | `providers` |
| `set_git_provider` | `name`, `web_url`, optional `api_url`, `git_name`, `git_email`, `route` | `provider` |
| `delete_git_provider` | `name` | |
| `list_git_credentials` | | `credentials` |
| `set_git_credential` | `name`, `provider`, optional `owner`, `repository`, `token` | `credential` |
| `check_git_credential` | `name` | `credential` |
| `delete_git_credential` | `name` | |
| `list_presets` | | `presets` |
| `set_preset` | `name`, `yaml` | `preset` |
| `delete_preset` | `name` | |
| `search_repositories` | `provider`, `query` | `repositories`, `more`, `errors` |
| `list_branches` | `provider`, `owner`, `repository` | `branches`, `default_branch`, `more` |
| `list_workspaces` | | `workspaces` |
| `create_workspace` | `name`, `host`, `repositories` (each with `url`, optional `provider`, `directory`, and `base_branch`), optional `volumes` | `workspace` |
| `update_workspace` | `name`, optional `host`, `repositories`, and `volumes` | `workspace` |
| `delete_workspace` | `name` | |
| `list_volumes` | | `volumes` |
| `create_volume` | `name`, `kind` (`incus` or `directory`), optional `size_mib` or `path` | `volume` |
| `delete_volume` | `name` | |
| `list_codespaces` | | `codespaces` |
| `create_codespace` | `workspace`, optional `preset`, `cpus`, `memory_mib`, `base_branch`, `base_branches` (directory to branch), `client_version` | `codespace`, `warnings` |
| `close_codespace` | `id` | `codespace` |

The environment of a host has `base_image`, `setup_script`, `variables`, `ca_certificates`, and `docker.registry_mirrors`. All are optional.

Each workspace mount has `volume`, `path`, and optional `read_only`. The volume list includes `hosts` and `deleting`.

Commands of hosts:

| Command | Fields | Use |
| --- | --- | --- |
| `host_report` | `report`: version, driver, CPUs, memory, and optional storage driver, Incus version, architecture, and maximum number of codespaces | The host sends it after each connection. |
| `host_instances` | `instances`: codespace, instance, oracle, running | The oracle deletes the instances that have no open codespace. It marks a codespace `lost` when its instance is missing. |
| `instance_progress` | `codespace`, `step` | A step of `create_instance`, for example the image build. |

Calls to hosts:

- `create_instance` sends the codespace, instance name, limits, environment, bootstrap file, agent version, and optional `volumes`. Each mount has `name`, `kind`, `path`, `read_only`, and optional `size_mib` or `source`.
- `delete_instance` and `list_instances` operate on instances.
- `check_workspace` sends `providers` and `directories`. The reply has a result for each item and `volumes: true` if the host supports volume calls.
- `provider_request` sends one API call to a provider.
- `delete_volume` sends `volume`, the name of the custom volume to remove.

Commands of codespace agents:

| Command | Fields | Result |
| --- | --- | --- |
| `subscribe` | | The agent gets `device_keys_changed` and `git_changed`. |
| `codespace_bundle` | | The repositories, the branch, the keys of the devices, the relay URL, the preset, and the Git setup of each provider. |
| `codespace_agent` | `address`: `host_id`, `host_key`, `instance` | The relay of the oracle accepts this agent. |
| `codespace_progress` | `step` | |
| `codespace_ready` | | |
| `codespace_failed` | `error` | |
| `git_credential` | `url` | `token` with `username`, `password`, and `expires_at`, or no token |

A codespace has one of these states: `provisioning`, `starting`, `ready`, `closing`, `closed`, `failed`, and `lost`.

The events are `device_changed`, `device_removed`, `pairing_changed`, `pairing_removed`, `relay_host_changed`, `relay_host_removed`, `host_changed`, `host_removed`, `workspace_changed`, `workspace_removed`, `codespace_changed`, `codespace_progress`, `codespace_removed`, `git_provider_changed`, `git_provider_removed`, `git_credential_changed`, `git_credential_removed`, `preset_changed`, and `preset_removed`. A closed codespace is removed with its workspace or its host. If a client reads the events too slowly, the oracle sends `events_lost`. Then the client reads the lists again.

Volume changes send `volume_changed` and `volume_removed` to devices.

The error codes are `bad_request`, `unknown_command`, `not_found`, `forbidden`, `conflict`, `unavailable`, and `internal`.

The oracle writes each change to the `audit` table of the store, with the peer that made it.
