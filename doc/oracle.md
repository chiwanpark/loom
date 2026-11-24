# Oracle

The oracle is the central service of Loom. The plan is in [the design document](design/loom-next.md).

The oracle does these things:

- It is the relay between the clients and the agents of the codespaces.
- It has a control service. Paired devices manage hosts, Git providers and credentials, model accounts, secrets, presets, workspaces, notes, and volumes through this service.
- It starts codespaces on hosts. See [Hosts and codespaces](host.md).
- It keeps its data in a SQLite store. It encrypts the secrets in the store with a master key.

Prebuilt binaries of `loom-oracle` are on the [releases page](https://github.com/chiwanpark/loom/releases). Build `loom-oracle` or `loom-oracle-cli` from source:

```sh
cargo build --release --bin loom-oracle --bin loom-oracle-cli
```

## Components

| Binary | Use |
| --- | --- |
| `loom-oracle` | The daemon. It has three commands: `serve`, `config`, and `pair`. |
| `loom-oracle-cli` | A command-line client for the control service. |

## Set up an oracle

1. Run `loom-oracle config init`. It writes `~/.config/loom/oracle.yaml`.
2. In `oracle.yaml`, set `url` to the URL that clients and agents use, for example `wss://oracle.example.com`.
3. Run `loom-oracle serve`. At the first start, the oracle makes its master key, its host keys, and its store.
4. Run `loom-oracle pair` on the same machine. It shows a one-time `loom://` link and a QR code for the first device.
5. On the device, run `loom-oracle-cli pair <link>`. `loom-oracle pair` stops when the device is paired.

After this, the first device makes the links for the other devices: on the **Devices** page of the oracle settings in the desktop client, or with `loom-oracle-cli pair-device`. You can do all other configuration in the desktop client too. See [Oracle settings](desktop.md#oracle-settings).

`loom-oracle config check` validates `oracle.yaml`. `loom-oracle config schema` shows its JSON Schema. `--config` or `LOOM_ORACLE_CONFIG` selects a different file.

## `oracle.yaml`

| Key | Meaning |
| --- | --- |
| `url` | The public URL of the oracle. Pairing links contain it. If it is not set, the oracle uses `ws://` or `wss://` with `listen`. |
| `listen` | The address of the relay and the control service, for example `127.0.0.1:8080`. |
| `metrics_listen` | The address of `/metrics`. See [Metrics](#metrics). |
| `tls` | The `cert` and the `key` files, for `wss://` without a proxy. |
| `cloudflare_access` | The `team_domain` and the `audience` of Cloudflare Access. The oracle then accepts only connections with a valid token. |
| `push` | The notifications for the Android app. See [Notifications for the Android app](#notifications-for-the-android-app). |
| `limits` | The limits of the relay, for example `max_frame` and `idle_timeout`. |
| `logging` | The log level. |
| `data_dir` | The directory of the store and the host keys. The default is `~/.local/share/loom/oracle`. |
| `master_key` | The file of the master key. The default is `master.key` in `data_dir`. |
| `codespaces` | The lifecycle of codespaces. See [Codespace lifecycle](#codespace-lifecycle). |

The relay of the oracle lets in only the agents of the codespaces. `oracle.yaml` has no `allowed_hosts`.

### Notifications for the Android app

The Android app gets notifications through Firebase Cloud Messaging. For this, the oracle needs the private key of a service account of the Firebase project of the app.

1. In the Firebase console, open **Project settings** and then **Service accounts**.
2. Click **Generate new private key**. The language of the code sample is not important.
3. Put the JSON file on the oracle machine. Only the oracle must be able to read it.
4. Add the file to `oracle.yaml`:

```yaml
push:
  fcm:
    service_account: /etc/loom/firebase-service-account.json
```

The Android app gives its push token to the oracle. The oracle gives the tokens to the agents of the codespaces. The agents send the notifications through the relay of the oracle.

The oracle has these metrics for notifications:

| Metric | Meaning |
| --- | --- |
| `loom_relay_pushes_sent_total` | Notifications that the oracle sent. |
| `loom_relay_pushes_rejected_total` | Notifications to tokens that Firebase does not know. The agents then forget these tokens. |

### Codespace lifecycle

| Key | Default | Meaning |
| --- | --- | --- |
| `codespaces.idle_stop` | `30m` | The oracle stops a ready codespace after this time without activity. `0s` turns it off. |
| `codespaces.retention` | `30days` | The oracle deletes a codespace after it is stopped for this time. `0s` turns it off. |
| `codespaces.closed_retention` | `7days` | The oracle removes closed codespaces from its list after this time. `0s` keeps them. |
| `codespaces.check_interval` | `1m` | The time between two checks of the idle stop and the retention. |
| `codespaces.archive` | `true` | The oracle keeps the sessions of a codespace when the codespace closes. See [Session archive](#session-archive). |
| `codespaces.archive_retention` | `90days` | The oracle deletes an archive and its closed codespace after this time. `0s` keeps them. |
| `codespaces.archive_max_size` | `1GiB` | The maximum size of the session files of one codespace. The oracle does not keep larger sessions. |

A codespace is active when a client is connected to its agent, a turn runs, or a terminal is open. The agent tells the oracle when this changes. A codespace that waits to start its first message is not idle.

Each host can set its own limits and timeouts. They replace the defaults. See [Codespace limits](#codespace-limits).

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
| `pair-host [--ttl <duration>] [--no-qr]` | Makes a one-time link for `loom-host join`. |
| `hosts` | Lists the hosts with their status and their report. |
| `remove-host <name>` | Removes a host. The oracle refuses this while it has workspaces, open codespaces, or tracked volumes. |
| `check-host <name> [--docker]` | Runs `loom-host check` on a connected host and shows the results. With `--docker`, the host also tests Docker in a test instance. |
| `set-ssh <host> <destination>`, `set-ssh <host> --clear` | Sets or removes the SSH destination of a host, for example `dev@host.example.com`. |
| `set-limits <host> [--max-codespaces <n>\|none] [--max-running <n>\|none] [--idle-stop <duration>\|never\|default] [--retention <duration>\|never\|default]` | Changes the limits of a host. The limits that you do not give stay the same. See [Codespace limits](#codespace-limits). |
| `host-volumes <name>` | Shows the used space of each custom volume on a connected host. |
| `set-environment <host> [--base-image <image>] [--setup-script <path>] [--env <name>=<value>]… [--ca-certificate <path>]… [--registry-mirror <url>]…` | Replaces the environment configuration of a host. The host builds a new image for the next codespace. |
| `providers` | Lists the Git providers. |
| `set-provider <name> --url <url> [--api-url <url>] [--git-name <name>] [--git-email <email>] [--route direct\|hosts]` | Adds or changes a Git provider. The default API URL is `https://api.github.com` for `https://github.com`, and `<url>/api/v3` for other servers. |
| `delete-provider <name>` | Deletes a Git provider. The oracle refuses this while a credential or a workspace uses it. |
| `credentials` | Lists the Git credentials. The list does not show the tokens. |
| `set-credential <name> --provider <provider> [--owner <owner> [--repository <repository>]] [--token-file <path>]` | Adds or changes a Git credential. `--token-file -` reads the token from standard input. A new credential needs a token. The oracle checks the token. |
| `check-credential <name>` | Checks the token again. |
| `delete-credential <name>` | Deletes a Git credential. The oracle refuses this while a preset uses it. |
| `presets` | Lists the presets. |
| `set-preset <name> <path> [--agents-md <path>] [--skills <directory>]` | Adds or changes a preset from an `agent.yaml` file. `--agents-md` sets `AGENTS.md`. `--skills` sets the skills from the skill directories in this directory. Without these options, the preset keeps its `AGENTS.md` and skills. |
| `show-preset <name>` | Shows a preset with its YAML file, `AGENTS.md`, and skill files. |
| `delete-preset <name>` | Deletes a preset. The oracle refuses this while an open codespace uses it. |
| `accounts` | Lists the model accounts. The list does not show the credentials. |
| `sign-in <account> --provider <provider> [--device-code]` | Signs in a model account. The CLI shows a URL or a device code. |
| `set-api-key <account> --provider <provider> [--key-file <path>]` | Adds or changes a model account with an API key. The default `--key-file -` reads the key from standard input. |
| `delete-account <name>` | Deletes a model account. The oracle refuses this while a preset uses it. |
| `models <preset>` | Lists the models that a preset can use, with the account of each model. |
| `secrets` | Lists the secrets. The list does not show the values. |
| `set-secret <name> [--value-file <path>]` | Adds or changes a secret. The default `--value-file -` reads the value from standard input. |
| `delete-secret <name>` | Deletes a secret. The oracle refuses this while a preset uses it. |
| `notes <workspace>` | Shows the notes of a workspace. |
| `set-notes <workspace> <path>` | Replaces the notes of a workspace. `-` reads them from standard input. |
| `sessions [--codespace <id>]` | Lists the sessions of the codespaces. |
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
| `create-codespace <workspace> [--preset <preset>] [--cpus <n>] [--memory <size>] [--branch <branch>] [--base <directory>=<branch>]… [--message <text> \| --message-file <path>] [--model <provider>/<model>] [--no-wait]` | Makes a codespace and shows its steps until it is ready. The defaults are 2 CPUs and 4GiB. `--branch` sets the base branch of all repositories, and `--base` sets it for one repository. `--message` sets the first message: the agent starts a session with it when the codespace is ready. `--model` sets the model of this session. |
| `close-codespace <id> [--push <directory>]… [--discard <directory>]… [--message <message>] [--force] [--no-wait]` | Closes a codespace. The host deletes its instance. First, the close guard asks the agent for the unsaved work of each repository. See [the host document](host.md#git-in-a-codespace). |
| `stop-codespace <id> [--no-wait]` | Stops a codespace. The host stops its instance and keeps its disk. |
| `resume-codespace <id> [--no-wait]` | Starts a stopped codespace again and waits until it is ready. |
| `archive <id> <session>` | Shows an archived session of a closed codespace as text. With `--json`, it writes the events. `sessions --codespace <id>` lists the sessions. |
| `delete-archive <id>` | Deletes the session archive of a closed codespace. |
| `connections <id> [--type relay\|ssh]…` | Shows how a client can connect to a codespace. |
| `repositories <id>` | Shows the state of each repository of a codespace. |
| `push <id> [--message <message>] [--directory <directory>]…` | Pushes the branch of each repository of a codespace. With `--message`, the agent first commits the uncommitted changes. |
| `pull-request <id> --title <title> [--body <text> \| --body-file <path>] [--draft]` | Opens a pull request for each repository of a codespace that has commits. |
| `audit [--limit <n>] [--before <id>]` | Shows the audit log, newest first. The default limit is 100. `--before` shows the entries before an entry. |
| `watch [--count <n>]` | Shows the events of the oracle until you stop it. |

With `--json`, each command writes JSON.

If the oracle is behind Cloudflare Access, pair with `--access-id-env <name> --access-secret-env <name>`. These are the names of the environment variables that contain the service token. The CLI keeps the names, not the values.

## Limits

The oracle uses the limits from `limits` in `oracle.yaml`. A connection uses the limits of its IP address until it proves its identity:

- An agent uses its own limits after it registers.
- A device uses its own limits after it opens a channel to the control service.

Thus, many agents and devices behind one public IP address do not share one limit. Before this change, all limits applied to each IP address.

## Codespace limits

Each host has these limits. Set them with `loom-oracle-cli set-limits`, or in **Settings > Hosts > Limits…** of the desktop app.

| Limit | Meaning |
| --- | --- |
| Maximum codespaces | The maximum number of open codespaces, running or stopped. `max_codespaces` in `host.yaml` also applies. The smaller value applies. |
| Maximum running codespaces | The maximum number of running codespaces. To create or resume a codespace, stop or close a different one first. |
| Idle stop | Replaces `codespaces.idle_stop` for the codespaces of the host. `never` turns it off. |
| Retention | Replaces `codespaces.retention` for the codespaces of the host. `never` keeps stopped codespaces. |

A list of codespaces shows when the oracle stops each idle codespace and when it deletes each stopped codespace.

## Metrics

When `metrics_listen` is set, `/metrics` gives the metrics of the relay and these gauges:

| Metric | Meaning |
| --- | --- |
| `loom_oracle_codespaces{state}` | The codespaces in each state. |
| `loom_oracle_codespaces_active` | The ready codespaces with a client, a running turn, or a terminal. |
| `loom_oracle_codespace_sessions` | The sessions in running codespaces. |
| `loom_oracle_codespace_busy_sessions` | The sessions with a running turn. |
| `loom_oracle_codespace_connections` | The client connections to running codespaces. |
| `loom_oracle_hosts_connected` | The connected hosts. |
| `loom_oracle_devices_connected` | The connected devices. |
| `loom_oracle_host_codespaces_running{host}` | The running codespaces of each host. |
| `loom_oracle_host_cpus_limit{host}` | The sum of the CPU limits of the running codespaces of each host. |
| `loom_oracle_host_memory_limit_bytes{host}` | The sum of the memory limits of the running codespaces of each host. |

The agents send their session metrics every 5 minutes and when their activity changes.

## Audit log

The oracle records each change: pairings, devices, hosts, workspaces, presets, credentials, and codespaces. The entries show the device that made the change. Entries without a device are changes of the oracle itself, such as an idle stop (`codespace_idle_stopped`) or a deletion after the retention (`codespace_expired`). Read the log with `loom-oracle-cli audit` or in **Settings > Audit log** of the desktop app.

## Session archive

When a codespace closes, the oracle keeps a read-only copy of its sessions. This occurs when you close a codespace and when the oracle deletes a codespace after `codespaces.retention`.

1. The oracle stops the instance, so that the session files of the agent are complete.
2. The host copies the session files from `~/.local/share/loom/sessions` in the instance to `archive/` in its data directory.
3. The oracle reads the copies in parts of 1 MiB and checks their size and SHA-256 checksum. It keeps them in `archive/<codespace>` in its data directory.
4. The host deletes the instance and the copies.

If the host is not connected, the archive and the delete wait for the host. If the archive fails for a different cause, for example because the sessions are larger than `codespaces.archive_max_size`, the host deletes the instance and the codespace shows the error.

`codespaces --all` shows the size of each archive and when the oracle deletes it. In the desktop app, open **Archive** below a workspace. Click a closed codespace to read its last session, or select a session in its menu. A lost codespace has no archive, because its instance is not there.

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

A preset is an agent configuration with a name. It has an `agent.yaml` file, an optional `AGENTS.md`, and optional skills. The oracle validates the preset when you save it.

The YAML file can name credentials of the oracle:

```yaml
llm:
  default_model: anthropic/claude-sonnet-4-5
  accounts:
    anthropic: work
git:
  credentials: [ghe-work, github-personal]
environment:
  secrets:
    NPM_TOKEN: npm-work
```

- `llm.accounts` gives the model account of each provider. A codespace can use only the providers that have an account. The account must be of the same provider.
- `git.credentials` is a list of Git credential names.
- `environment.secrets` gives the secret of each environment variable. Two variables can use the same secret.

All names must exist when you save the preset. The oracle refuses to delete a model account, a Git credential, or a secret while a preset uses it.

The agent sets some sections itself: `allowed_roots`, `socket`, `storage`, `lifecycle`, `relay.enabled`, `relay.url`, and `environment.env_file`. A preset with one of these sections is not valid. In a preset, the default of `policy.default` is `allow`, because the codespace is the sandbox. The file can set `policy.default: ask`.

The skills are files in skill directories, for example `skills/review/SKILL.md` and `skills/review/scripts/run.sh`. Each skill directory must have a `SKILL.md` file. A preset has up to 200 skill files with up to 512KiB together.

When a preset, a model account, or a secret of a preset changes, the oracle sends `config_changed` with the names of the presets. The codespaces of these presets read the new configuration. New sessions use it at once.

### Model accounts

A model account is a sign-in or an API key for one model provider. It has a name and a provider. The oracle keeps the credentials sealed with the master key and refreshes the tokens.

```sh
loom-oracle-cli sign-in work --provider anthropic
loom-oracle-cli set-api-key team --provider openai --key-file key.txt
```

A provider with a sign-in, such as `anthropic` or `openai-codex`, can have a sign-in account. A custom provider, such as one of type `openai-compatible`, needs an API key account.

A codespace never gets a refresh token. The agent asks the oracle for an access token that is valid for 30 minutes or more. It asks again before the token expires. The oracle refreshes the token of an account only once at a time, so many codespaces can use one account. In a codespace, the agent cannot sign in or sign out. Its status shows the name of the account.

### Git in a codespace

For each repository, the codespace uses the credential of the preset with the smallest scope that matches. If two credentials have the same scope, the first in the list wins.

The codespace configures Git for each provider:

- The Git credential helper asks the oracle for the token of each repository when Git needs it. Git does not keep the token.
- `gh` uses the credential of the first repository of the provider. The codespace writes this token to the configuration file of `gh`.
- The Git name and email come from the provider. If the provider has no name, the codespace uses the login of the credential. If the provider is github.com and has no email, the codespace uses the noreply address of the login. For other providers, set the email on the provider. When the codespace has no email for a provider, the oracle shows a warning.

When a provider, a credential, or a preset changes, the oracle sends `git_changed` to the codespaces. They read the new setup.

### Agent configuration in a codespace

The agent of a codespace writes its preset to these files:

- `agent.yaml` goes to `~/.config/loom/agent.yaml`, and `AGENTS.md` goes to `~/.config/loom/AGENTS.md`.
- The skills go to `~/.config/loom/skills/`.
- The secrets go to `~/.local/share/loom/secrets.env`, with mode 0600. The agent reads this file as its environment file. The model sees only the variable names.

The agent also reads `AGENTS.md`, `.loom/skills/`, and `.agents/skills/` of each repository of the workspace.

The oracle keeps the notes of each workspace. A new codespace gets the notes of its workspace. When the agent changes its notes, it sends them to the oracle. The agent also sends a summary of each session: the title, the state, and the model.

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
| `create_host_pairing` | `ttl_secs` (optional) | `pairing`, `link` |
| `list_hosts` | | `hosts` |
| `remove_host` | `name` | |
| `set_host_environment` | `name`, `environment` | `host` |
| `set_host_ssh` | `name`, optional `destination` | `host` |
| `check_host` | `name`, optional `docker` | `host` with `checks` and `checked_at` |
| `host_volumes` | `name` | `volumes`, each with `name`, `used_bytes`, `total_bytes`, or `error` |
| `list_git_providers` | | `providers` |
| `set_git_provider` | `name`, `web_url`, optional `api_url`, `git_name`, `git_email`, `route` | `provider` |
| `delete_git_provider` | `name` | |
| `list_git_credentials` | | `credentials` |
| `set_git_credential` | `name`, `provider`, optional `owner`, `repository`, `token` | `credential` |
| `check_git_credential` | `name` | `credential` |
| `delete_git_credential` | `name` | |
| `list_presets` | | `presets` |
| `set_preset` | `name`, `yaml`, optional `agents_md` and `files` (each with `path` and `content`) | `preset` |
| `get_preset` | `name` | `preset`, `files` |
| `delete_preset` | `name` | |
| `list_model_accounts` | | `accounts` |
| `start_model_login` | `account`, `provider`, `method` (`browser` with optional `redirect_uri`, or `device_code`) | `login`, `account`, `provider`, `prompt` |
| `complete_model_login` | `login`, `input` (`redirect` with `url`, or `code` with `code`) | `account` |
| `cancel_model_login` | `login` | |
| `set_model_api_key` | `account`, `provider`, `api_key` | `account` |
| `delete_model_account` | `name` | |
| `list_models` | `preset` | `models`, `default_model`, `errors` |
| `list_secrets` | | `secrets` |
| `set_secret` | `name`, `value` | `secret` |
| `delete_secret` | `name` | |
| `get_workspace_notes`, `set_workspace_notes` | `workspace`, and `text` to set | `workspace`, `text`, `updated_at` |
| `list_sessions` | optional `codespace` | `sessions` |
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
| `create_codespace` | `workspace`, optional `preset`, `cpus`, `memory_mib`, `base_branch`, `base_branches` (directory to branch), `client_version`, `first_message` (`text` and optional `model`) | `codespace`, `warnings` |
| `codespace_warnings` | `workspace`, optional `preset` | `warnings`: the warnings that `create_codespace` gives for this workspace and preset |
| `close_codespace` | `id` | `codespace` |

The environment of a host has `base_image`, `setup_script`, `variables`, `ca_certificates`, and `docker.registry_mirrors`. All are optional.

Each workspace mount has `volume`, `path`, and optional `read_only`. The volume list includes `hosts` and `deleting`.

Commands of hosts:

| Command | Fields | Use |
| --- | --- | --- |
| `host_report` | `report`: version, driver, CPUs, memory, and optional storage driver, Incus version, architecture, maximum number of codespaces, and `checks` | The host sends it after each connection. The checks are the results of `loom-host check` without the Docker test. |
| `host_instances` | `instances`: codespace, instance, oracle, running | The oracle deletes the instances that have no open codespace. It marks a codespace `lost` when its instance is missing. |
| `instance_progress` | `codespace`, `step` | A step of `create_instance`, for example the image build. |

Calls to hosts:

- `create_instance` sends the codespace, instance name, limits, environment, bootstrap file, agent version, and optional `volumes`. Each mount has `name`, `kind`, `path`, `read_only`, and optional `size_mib` or `source`.
- `delete_instance` and `list_instances` operate on instances.
- `check_workspace` sends `providers` and `directories`. The reply has a result for each item and `volumes: true` if the host supports volume calls.
- `provider_request` sends one API call to a provider.
- `delete_volume` sends `volume`, the name of the custom volume to remove.
- `check` sends `docker`. The reply has `checks`, each with `name`, `status` (`ok`, `warning`, or `failed`), and `detail`.
- `volume_usage` sends `volumes`, the names of custom volumes. The reply has the used space of each volume.

Commands of codespace agents:

| Command | Fields | Result |
| --- | --- | --- |
| `subscribe` | | The agent gets `device_keys_changed`, `git_changed`, and `config_changed`. |
| `codespace_bundle` | | The repositories, the branch, the keys and the push tokens of the devices, the relay URL, the preset, the Git setup of each provider, the agent configuration with the secret values, and the notes. |
| `codespace_agent` | `address`: `host_id`, `host_key`, `instance` | The relay of the oracle accepts this agent. |
| `codespace_progress` | `step` | |
| `codespace_ready` | | |
| `codespace_failed` | `error` | |
| `git_credential` | `url` | `token` with `username`, `password`, and `expires_at`, or no token |
| `model_credentials` | | `sections` (the credential of each provider, without refresh tokens), `accounts`, `valid_secs`, `errors` |
| `codespace_notes` | `text` | |
| `session_summaries` | `sessions`, `removed`, `full` | With `full`, the oracle removes the sessions that are not in the list. |
| `take_first_message` | | `message`, or no message. The oracle gives the first message only once. |

A codespace has one of these states: `provisioning`, `starting`, `ready`, `closing`, `closed`, `failed`, and `lost`.

The events are `device_changed`, `device_removed`, `pairing_changed`, `pairing_removed`, `host_changed`, `host_removed`, `workspace_changed`, `workspace_removed`, `codespace_changed`, `codespace_progress`, `codespace_removed`, `git_provider_changed`, `git_provider_removed`, `git_credential_changed`, `git_credential_removed`, `preset_changed`, and `preset_removed`. A closed codespace is removed with its workspace or its host. If a client reads the events too slowly, the oracle sends `events_lost`. Then the client reads the lists again.

Volume changes send `volume_changed` and `volume_removed` to devices.

Other events for devices are: `model_account_changed`, `model_account_removed`, `model_login_finished`, `secret_changed`, `secret_removed`, `workspace_notes_changed`, `session_changed`, `session_removed`, and `config_changed`. A device sign-in with a device code ends with `model_login_finished`.

The error codes are `bad_request`, `unknown_command`, `not_found`, `forbidden`, `conflict`, `unavailable`, and `internal`.

The oracle writes each change to the `audit` table of the store, with the peer that made it.
