# Hosts and codespaces (Loom-Next)

A host runs the codespaces of the oracle. The daemon `loom-host` connects out to the oracle and starts and deletes the codespaces of the host. The plan is in [the design document](design/loom-next.md). This page describes the state after phase 2.

In phase 2, a workspace has one public repository. A codespace is an Incus container. In the container, the agent clones the repository, makes the branch `loom/<codespace>`, and registers with the relay of the oracle. The current desktop client connects to the agent through this relay.

The releases do not contain `loom-host` yet. Build it from the branch `loom-next`. The host also needs a build of `loom-agent` from this branch, because the managed mode is new:

```sh
cargo build --release --bin loom-host --bin loom-agent
```

## Drivers

| Driver | Use |
| --- | --- |
| `incus` | The default. Each codespace is a container in an Incus project. `loom-host` uses the local Incus socket, or the HTTPS API with a client certificate. |
| `process` | Starts each codespace agent as a local process of the user of `loom-host`. There is no container and no limit. Use it for tests and for the development of Loom. |

The `incus` driver needs Incus 6.0 or later. The tests used Incus 6.0.4 with the `dir` pool and Incus 7.6 on IncusOS with ZFS.

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

## Make a codespace

With `loom-oracle-cli`:

```sh
loom-oracle-cli create-workspace acme --host <host> --repository https://github.com/acme/app.git
loom-oracle-cli create-codespace acme --cpus 2 --memory 4GiB
```

`create-codespace` shows the steps until the codespace is ready. Then it shows a host entry for `client.yaml` of the desktop client. `loom-oracle-cli client-entry <id>` shows the entry again.

The device keys of the oracle are the keys that the agent of a codespace accepts. Thus, the desktop client must use a key that is paired with the oracle. Pair the key of the desktop client with `loom-oracle-cli pair <link> --identity ~/.local/share/loom/client_identity.json`.

`loom-oracle-cli close-codespace <id>` deletes the container of the codespace and revokes the keys of its agent.

## Codespace agents

In a codespace, `loom-agent serve --managed` runs the agent:

1. The bootstrap file `~/.config/loom/bootstrap.json` of the agent contains a one-time link for this codespace.
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
