# Hosts and codespaces (Loom-Next)

A host runs the codespaces of the oracle. The daemon `loom-host` connects out to the oracle and starts and deletes the codespaces of the host. The plan is in [the design document](design/loom-next.md). This page describes the state after phase 4.

A workspace has one or more repositories and can have persistent volumes. A codespace is an Incus container. In the container, the agent clones the repositories with the Git credentials of its preset, makes the branch `loom/<codespace>` in each clone, and registers with the relay of the oracle. The current desktop client connects to the agent through this relay.

The releases do not contain `loom-host` yet. Get `loom-host` and `loom-agent` from the CI archives for the same `loom-next` build, or build them:

```sh
cargo build --release --bin loom-host --bin loom-agent
```

## Drivers

| Driver | Use |
| --- | --- |
| `incus` | The default. Each codespace is a container in an Incus project. `loom-host` uses the local Incus socket, or the HTTPS API with a client certificate. |
| `process` | Starts each codespace agent as a local process of the user of `loom-host`. There is no container and no limit. Use it for tests and for the development of Loom. |

The `incus` driver needs Incus 6.0 or later. The tests used Incus 6.0.4 with the `dir` pool and Incus 7.6 on IncusOS with ZFS.
Custom volumes also need the `storage_initial_owner` Incus API extension (Incus 6.0.3 or later in the 6.0 series).

## Set up a Linux host

On a Linux host, `loom-host` runs as a systemd service and uses the local Incus socket.

1. Install Incus and initialize it, for example with `sudo incus admin init --minimal`. Incus must have a storage pool and a bridge network.
2. Make the user and install the binaries:

   ```sh
   sudo useradd --system --home-dir /var/lib/loom-host --create-home --shell /usr/sbin/nologin loom-host
   sudo usermod -aG incus-admin loom-host
   sudo install -m755 loom-host loom-agent /usr/local/bin/
   sudo loom-host --config /etc/loom/host.yaml config init
   ```

3. In `/etc/loom/host.yaml`, set `data_dir: /var/lib/loom-host`, `incus.pool`, `incus.network`, and `agent.path: /usr/local/bin/loom-agent`.
4. Make the Incus project. `loom-host setup` makes the restricted project `incus.project` if it does not exist:

   ```sh
   sudo -u loom-host loom-host --config /etc/loom/host.yaml setup
   ```

5. On a device of the oracle, run `loom-oracle-cli pair-host`. It shows a one-time link. Then join the oracle and check the host:

   ```sh
   sudo -u loom-host loom-host --config /etc/loom/host.yaml join '<link>' --name <name>
   sudo -u loom-host loom-host --config /etc/loom/host.yaml check --docker
   ```

6. Start the service:

   ```sh
   sudo install -m644 contrib/systemd/loom-host.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now loom-host.service
   ```

`loom-oracle-cli hosts` then shows the host as `connected`.

The members of `incus-admin` have full control of Incus. If you do not want this, use a restricted client certificate as in the next section, with the HTTPS address of the local Incus.

## Set up a host with IncusOS or a remote Incus

`loom-host` can run on a different machine and use the HTTPS API of Incus with a restricted client certificate. This is the setup for IncusOS.

1. On a machine that controls Incus, make the project and a trust token for this project only:

   ```sh
   incus project create loom -c restricted=true -c restricted.containers.nesting=allow \
     -c restricted.networks.access=incusbr0 -c features.images=true \
     -c features.profiles=true -c features.storage.volumes=true
   incus config trust add loom-host --restricted --projects loom
   ```

2. On the machine of `loom-host`, set `driver: incus` and `incus.url`, for example `https://incus.example.com:8443`, in `host.yaml`. Also set `incus.pool` and `incus.network`.
3. Run `loom-host trust '<token>'`. It does these steps:
   - It connects to the URL, and compares the server certificate with the fingerprint in the token.
   - It makes a client key and certificate in `incus/` in the data directory.
   - It keeps the server certificate there, and then accepts only this certificate.
   - It adds the client certificate to Incus with the token.
4. Join the oracle, check the host, and start `loom-host serve`, as for a Linux host.

`--no-pin` uses the CA certificates of the system, and does not keep the server certificate. `incus.client_certificate`, `incus.client_key`, and `incus.server_certificate` select different files.

## `host.yaml`

| Key | Meaning |
| --- | --- |
| `driver` | `incus` or `process`. The default is `incus`. |
| `data_dir` | The directory of the key, the address of the oracle, the Incus certificates, and the agent binaries. The default is `~/.local/share/loom/host`. |
| `incus.socket` | The Incus socket. The default is `/var/lib/incus/unix.socket` or `/run/incus/unix.socket`. |
| `incus.url` | The HTTPS API of Incus. If it is set, `loom-host` does not use the socket. |
| `incus.client_certificate`, `incus.client_key`, `incus.server_certificate` | The certificates for `incus.url`. The defaults are in `incus/` in the data directory. |
| `incus.project` | The Incus project of the codespaces. The default is `loom`. |
| `incus.pool` | The storage pool. The default is `default`. |
| `incus.network` | The bridge network. The default is `incusbr0`. |
| `process.root` | The directory of the codespaces of the `process` driver. The default is `codespaces` in `data_dir`. |
| `agent.path` | The `loom-agent` binary for the codespaces. If it is not set, the host downloads the release of the version of each client and keeps it in `agents/` in the data directory. |
| `rules.max_codespaces` | The maximum number of codespaces on this host. The oracle cannot change this rule. |
| `rules.directories` | The allowed absolute paths for host directory volumes. Each path allows that directory and its descendants. |
| `rules.host_user` | The non-root host `uid` and `gid` that Incus maps to `dev` (1000:1000). |
| `report_interval` | How often the host sends its instances to the oracle. The default is `60s`. |
| `logging.level` | The log level. |

`loom-host config check` validates the file, and `loom-host config schema` shows its JSON Schema. `--config` or `LOOM_HOST_CONFIG` selects a different file.

The releases before Loom-Next do not have the managed mode. Until a release has it, set `agent.path` to a build from the branch `loom-next`.

## The codespace image

The `incus` driver builds an image for the codespaces of the host. It builds a new image when the environment of the host changes, and deletes the old images of the oracle after the build.

1. It starts a container from the base image. The default is `images:debian/13`. The base image can be `images:<alias>`, `ubuntu:<alias>`, or the alias of a local image. It must be Debian or Ubuntu.
2. It puts the environment variables into `/etc/loom/environment`, the CA certificates into `/usr/local/share/ca-certificates/`, and the registry mirrors into `/etc/docker/daemon.json`.
3. It runs the base script as root. The script installs Git, the GitHub CLI, curl, sudo, and Docker CE. It makes the user `dev` (UID 1000) with sudo and the `docker` group, and the directory `/workspaces`.
4. It runs the setup script of the host as root, if there is one.
5. It stops the container and publishes it as `loom-image-<hash>`, with zstd compression if Incus can use it.

Set the environment with `loom-oracle-cli set-environment`. The first codespace after a change waits for the build. On a test host with the `dir` pool, the first codespace took 50 seconds with the build. On the test hosts, a codespace from an existing image took 4 to 6 seconds.

`loom-host check --docker` starts a test container from the newest image, and runs `docker run hello-world` in it.

## Codespace containers

Each codespace is the container `loom-<codespace>`:

- It has the limits of the codespace (`limits.cpu`, `limits.memory`) and `security.nesting=true` for Docker.
- It has no profiles. Its root disk is on `incus.pool`, and its network interface is on `incus.network`.
- The keys `user.loom.oracle` and `user.loom.codespace` name its oracle and its codespace. `loom-host` changes only the containers with the key of its oracle.
- Before the start, `loom-host` puts in the `loom-agent` binary, the systemd unit of the agent, and the bootstrap file.

## Persistent volumes

The first codespace that uses a custom volume on a host creates `loom-<volume>` in `incus.pool`.
The new volume belongs to `dev`, and it stays after all codespaces close.
Codespaces on the same host share the data, but Loom does not copy it to another host.
Use a dedicated Incus project with `features.storage.volumes=true`.
If you specify a size limit, `loom-host check` reports pool support and checks for project quota mount options on a local `dir` pool when it can read the backing filesystem.

### Host directories

To mount an existing directory, use the local Incus socket. Do not use the HTTPS API for this setup.
List only directories that the host administrator intends to share. Use canonical paths. Keep the directories and their ancestors under administrator control.

```yaml
rules:
  directories: [/srv/codespace-data]
  host_user:
    uid: 12000
    gid: 12000
```

1. Set the UID and GID to those of a dedicated host user and group with access to the shared data.
2. As an administrator, make sure that `/etc/subuid` and `/etc/subgid` allow `root` to map these IDs. For this example, add `root:12000:1` to each file if its ranges do not already include the ID. Restart Incus after you change the ranges.
3. Run `loom-host setup` to apply the disk path and ID mapping rules to the Incus project. Then run `loom-host check`.

The mapping applies to all new codespaces on the host. A read-only mount cannot be used to write to the directory.
The process driver uses symlinks and permits mount paths only below `/workspaces/`. It does not apply quotas, read-only marks, or UID/GID mappings.

See [Workspace volumes](oracle.md#workspace-volumes) for commands to create, mount, and delete volumes.

## Make a codespace

With `loom-oracle-cli`:

```sh
loom-oracle-cli set-provider github --url https://github.com
loom-oracle-cli set-credential acme-token --provider github --owner acme --token-file - < token.txt
loom-oracle-cli sign-in work --provider anthropic
loom-oracle-cli set-preset work preset.yaml
loom-oracle-cli create-workspace acme --host <host> --repository github:acme/app --repository github:acme/lib
loom-oracle-cli create-codespace acme --preset work --cpus 2 --memory 4GiB --message "Fix the failing tests."
```

`preset.yaml` names the model account and the Git credentials:

```yaml
llm:
  default_model: anthropic/claude-sonnet-4-5
  accounts:
    anthropic: work
git:
  credentials: [acme-token]
```

`create-codespace` shows the steps until the codespace is ready. Then it shows a host entry for `client.yaml` of the desktop client. `loom-oracle-cli client-entry <id>` shows the entry again. With `--message`, the agent starts a session with this message when the codespace is ready. A client does not need to be connected.

The device keys of the oracle are the keys that the agent of a codespace accepts. Thus, the desktop client must use a key that is paired with the oracle. Pair the key of the desktop client with `loom-oracle-cli pair <link> --identity ~/.local/share/loom/client_identity.json`.

`loom-oracle-cli close-codespace <id>` deletes the container of the codespace and revokes the keys of its agent. Before that, the close guard checks the unsaved work. See [Git in a codespace](#git-in-a-codespace).

## Git providers

The oracle asks each connected host which Git providers it reaches. The host sends a request to the API URL of each provider. Any HTTP answer counts as a reach. `loom-oracle-cli hosts` shows the result.

For a provider with the route `hosts`, the oracle sends API calls through a host that reaches the provider, for example the search of repositories. The host sends each call to the API URL that the oracle gives. It sends only `GET`, `POST`, and `PATCH` requests to paths in the API.

See [the oracle document](oracle.md#git-providers) for providers, credentials, and presets.

## Codespace agents

In a codespace, `loom-agent serve --managed` runs the agent:

1. The bootstrap file `~/.config/loom/bootstrap.json` of the agent contains a one-time link for this codespace.
2. The agent makes its keys and pairs with the oracle. Then it deletes the bootstrap file. It keeps the address of the oracle in `managed.json` in its data directory, so it can start again without the link.
3. The agent sets up Git for the providers of the preset, clones each repository, and makes the branch `loom/<codespace>` in each clone.
4. The agent writes the agent configuration of its preset and gets the model credentials from the oracle. See [Agent configuration in a codespace](oracle.md#agent-configuration-in-a-codespace). If the configuration is not valid, the codespace fails.
5. The agent accepts the keys of the devices of the oracle. When you pair or revoke a device, the agent gets the change.
6. The agent registers with the relay of the oracle and reports `ready`. If the codespace has a first message, the agent then starts a session with it.

`--bootstrap <path>` selects a different bootstrap file.

| Workspace | Directory of each clone | Root directory of the agent |
| --- | --- | --- |
| One repository | `/workspaces/<directory>` | The clone. |
| More than one repository | `/workspaces/<workspace>/<directory>` | `/workspaces/<workspace>` |

The Git setup of a codespace is in the data directory of the agent (`~/.local/share/loom/git`). The global Git configuration of the user includes the file `git/config`:

- For each provider with a credential in the preset, `loom-agent git-credential` is the only credential helper. It asks the agent through the socket `git/credential.sock`, and the agent asks the oracle.
- Each clone has the Git name and email of its provider in its own configuration. Other clones of a provider get them through `includeIf "hasconfig:remote.*.url:…"`.
- The agent writes the token for `gh` of each provider into `~/.config/gh/hosts.yml`. It keeps the hosts that it did not write.

When the oracle sends `git_changed`, the agent writes these files again.

## Git in a codespace

A client asks the agent of a codespace for these Git actions:

| Action | `loom-oracle-cli` | What the agent does |
| --- | --- | --- |
| Status | `repositories <id>` | Shows the branch, the base branch, the changed files, the commits after the base, and the commits that are not pushed, for each clone. |
| Push | `push <id>` | Pushes the branch of each clone that has commits that are not pushed. With `--message`, it first commits all changes. |
| Pull request | `pull-request <id> --title <title>` | For each clone of a provider with commits after the base, pushes the branch and opens a pull request into the base. If the branch has an open pull request, the agent uses it. When it opens more than one, it adds links to the others to each body. |

The agent sends the API calls for pull requests to the provider with the token that the oracle selects for the repository. It does not use `gh`. A repository without a provider gets no pull request.

The close guard runs before a codespace closes. It asks the agent for the state of each clone. A clone has unsaved work when it has changed files or commits that are not pushed. For each repository with unsaved work, choose one:

- `--push <directory>` pushes the branch. If the clone has changed files, also give `--message` to commit them.
- `--discard <directory>` loses the work.

`*` selects all repositories. The close stops if a repository with work has no choice.

If the agent does not answer, the close stops. `--force` closes the codespace without the check. A lost codespace also needs `--force`.

## Codespace states

The oracle compares the instances that the host reports with its codespaces:

- It deletes an instance that has no open codespace.
- A codespace whose instance is missing for more than 60 seconds becomes `lost`.
- If the host was not connected when you closed a codespace, the oracle deletes the instance after the host connects again.
