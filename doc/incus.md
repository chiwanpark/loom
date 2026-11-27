# Incus deployment

The oracle and `loom-host` can run as Incus system containers. Then the machines need no Loom users, no systemd units, and no Loom binaries outside the containers. Only the clients run outside Incus.

The commands in this document come from the Incus documentation. Replace the values in angle brackets, for example `<pool>`.

## Layout

| Incus project | Instances | Who controls it |
| --- | --- | --- |
| `default` | The container `loom-oracle` on one machine. The container `loom-host` on each machine with codespaces. | The Incus administrator. |
| `loom` (restricted) | The codespaces of the host. | `loom-host`, with a client certificate that only this project trusts. |

The oracle does not use the Incus API. A machine for the oracle only needs Incus. It can be a small arm64 computer.

Keep the codespaces in the restricted project `loom`. Do not give `loom-host` a certificate for `default`: such a certificate can control all instances in `default` and mount host paths.

## Images

Each release has two images for `amd64` and for `arm64`. Each image is a metadata tarball and a rootfs tarball:

- `loom-oracle-<version>-incus-<arch>.metadata.tar.xz` and `loom-oracle-<version>-incus-<arch>.rootfs.tar.xz`
- `loom-host-<version>-incus-<arch>.metadata.tar.xz` and `loom-host-<version>-incus-<arch>.rootfs.tar.xz`

The images have Alpine Linux and the static binaries of the release. The `loom-host` image also has `loom-agent` for the codespaces, and the SSH server dropbear.

Download the images, compare them with `SHA256SUMS`, and import them into the project `default` of each machine:

```sh
gh release download v<version> --repo chiwanpark/loom --pattern '*-incus-<arch>.*' --pattern SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
incus image import loom-host-<version>-incus-<arch>.metadata.tar.xz loom-host-<version>-incus-<arch>.rootfs.tar.xz --alias loom-host
```

A container from these images does these steps:

1. It gets its IPv4 address with DHCP.
2. It waits until a volume is mounted at `/var/lib/<component>`. Until then, it writes a message to the console every 30 seconds. `incus console <component> --show-log` shows it.
3. It starts `<component> serve --wait`. The program waits until its configuration is ready, and then serves. If it stops, the container starts it again after 5 seconds.

These paths are in `/var/lib/<component>`, on the volume:

| Path | Contents |
| --- | --- |
| `config/loom/` | `oracle.yaml` or `host.yaml`. |
| `data/loom/` | The data directory. |
| `log/` | The logs. When a log is larger than 10 MiB, the container moves it to `<name>.log.1`. |
| `ssh/` | `loom-host` only: the host key and `authorized_keys` of the SSH server. |

`/root/.config`, `/root/.local/share`, and `/root/.local/state` are links to `config`, `data`, and `state` on the volume. Thus, `incus exec` and the service use the same files.

## Oracle

1. Make two custom volumes. One is for the store and the session archives. The other is for the master key, so that you can keep the key apart from the backups of the store:

   ```sh
   incus storage volume create <pool> loom-oracle-data
   incus storage volume create <pool> loom-oracle-key
   incus storage volume set <pool> loom-oracle-data snapshots.schedule=@daily snapshots.expiry=14d
   ```

2. Make the container and attach the volumes:

   ```sh
   incus create loom-oracle loom-oracle
   incus storage volume attach <pool> loom-oracle-data loom-oracle loom-oracle-data /var/lib/loom-oracle
   incus storage volume attach <pool> loom-oracle-key loom-oracle loom-oracle-key /var/lib/loom-oracle-key
   incus config set loom-oracle boot.autostart=true
   incus start loom-oracle
   ```

3. Write `oracle.yaml`:

   ```sh
   incus exec loom-oracle -- loom-oracle config init
   incus file edit loom-oracle/var/lib/loom-oracle/config/loom/oracle.yaml
   ```

   Set these keys:

   ```yaml
   url: wss://<oracle hostname>
   listen: 127.0.0.1:8090
   master_key: /var/lib/loom-oracle-key/master.key
   ```

   On a machine with little disk space, also set smaller values for `codespaces.archive_max_size` and `codespaces.archive_retention`. See [Session archive](oracle.md#session-archive).

   The oracle starts when the file is valid. At the first start, it makes the master key, its host keys, and its store.

4. Let the clients and the agents reach the oracle. A proxy device sends a port of the machine to the oracle in the container. TLS ends at a proxy in front of it, for example a Cloudflare tunnel:

   ```sh
   incus config device add loom-oracle web proxy listen=tcp:<machine address>:8090 connect=tcp:127.0.0.1:8090
   ```

   Point the proxy or the tunnel for `<oracle hostname>` to `http://<machine address>:8090`.

5. Pair the first device:

   ```sh
   incus exec loom-oracle -- loom-oracle pair
   ```

6. Keep a copy of the master key in a safe place:

   ```sh
   incus file pull loom-oracle/var/lib/loom-oracle-key/master.key ./loom-oracle-master.key
   ```

The log is `/var/lib/loom-oracle/log/loom-oracle.log`:

```sh
incus exec loom-oracle -- tail -f /var/lib/loom-oracle/log/loom-oracle.log
```

## Host

### Prepare Incus

Do these steps as the Incus administrator on each machine with codespaces.

1. Make a bridge for the codespaces. Then the codespaces are not on the bridge of the other instances:

   ```sh
   incus network create loombr0
   ```

2. The host routes between its bridges. Make a network ACL that rejects the traffic from the codespaces to the other bridges, and to the Incus API and the SSH port on the addresses of the host. Keep DNS and DHCP on the gateway of `loombr0` open:

   ```sh
   incus network acl create loom-codespaces
   incus network acl rule add loom-codespaces egress action=reject destination=<subnet of incusbr0>
   incus network acl rule add loom-codespaces egress action=reject protocol=tcp \
     destination=<addresses of the host> destination_port=8443,2222
   ```

3. If the codespaces mount host directories, add the lines for `/etc/subuid` and `/etc/subgid`. See [Host directories](host.md#host-directories).

### Make the container

1. Make the volume and the container:

   ```sh
   incus storage volume create <pool> loom-host-data
   incus create loom-host loom-host
   incus storage volume attach <pool> loom-host-data loom-host loom-host-data /var/lib/loom-host
   incus config set loom-host boot.autostart=true
   incus start loom-host
   ```

2. Write `host.yaml`:

   ```sh
   incus exec loom-host -- loom-host config init
   incus file edit loom-host/var/lib/loom-host/config/loom/host.yaml
   ```

   ```yaml
   driver: incus
   incus:
     url: https://<address of Incus from the container>:8443
     project: loom
     pool: <pool>
     network: loombr0
   agent:
     path: /usr/local/bin/loom-agent
   rules:
     network:
       acls: [loom-codespaces]
   ```

   The container reaches Incus through the gateway of its bridge if `core.https_address` of the server includes it. For host directories, also set `rules.directories` and `rules.host_user`. See [Host directories](host.md#host-directories).

3. Show the commands for the Incus administrator, and run them on a machine that controls Incus. They make the restricted project `loom` with all its keys, and a trust token for this project:

   ```sh
   incus exec loom-host -- loom-host setup
   ```

4. Trust Incus, join the oracle, and check the host. Get the link with `loom-oracle pair --host`, or with **Add host…** on the **Hosts** page of the oracle settings:

   ```sh
   incus exec loom-host -- loom-host trust '<token>'
   incus exec loom-host -- loom-host join '<link>' --name <name>
   incus exec loom-host -- loom-host check --docker
   ```

`loom-host serve` starts when the host has joined. The **Hosts** page then shows the host as `connected`. The log is `/var/lib/loom-host/log/loom-host.log`.

For a Git provider with a company CA, put a file with all CA certificates on the volume, and set `environment.SSL_CERT_FILE` of the container to its path.

## SSH connections

The desktop client connects to a codespace over SSH when its host has an SSH destination. See [SSH connections](host.md#ssh-connections). The SSH server of the `loom-host` container does these things:

- It accepts only public keys from `/var/lib/loom-host/ssh/authorized_keys`, for the user `root`. It does not accept passwords.
- It runs only `loom-host stdio --codespace <id>` for each key. It refuses all other commands, shells, and port forwarding.
- It keeps its host key in `/var/lib/loom-host/ssh/`, so the key stays when you rebuild the container.

1. Add the public keys of the desktop clients:

   ```sh
   incus file push id_ed25519.pub loom-host/var/lib/loom-host/ssh/authorized_keys --uid 0 --gid 0 --mode 0600
   ```

2. Send a port of the machine to the SSH server. To keep the server off the network, listen on `127.0.0.1` and connect through the SSH server of the machine:

   ```sh
   incus config device add loom-host ssh proxy listen=tcp:127.0.0.1:2222 connect=tcp:127.0.0.1:22
   ```

3. On each desktop, add an entry to `~/.ssh/config`. `ProxyJump` can also name a jump host before the machine, for example `ProxyJump jump,<machine>`:

   ```text
   Host <name>-loom
       HostName 127.0.0.1
       Port 2222
       User root
       ProxyJump <machine>
       HostKeyAlias <name>-loom
   ```

4. Connect one time by hand to accept the host key: `ssh <name>-loom`. The connection then closes with a message, because the key can only run `loom-host stdio`.
5. Set the SSH destination of the host to `<name>-loom` on the **Hosts** page of the oracle settings.

The log of the SSH server is `/var/lib/loom-host/log/sshd.log`.

## Updates

Import the new image with the same alias, and rebuild the container. The volumes stay:

```sh
incus image import loom-host-<version>-incus-<arch>.metadata.tar.xz loom-host-<version>-incus-<arch>.rootfs.tar.xz --alias loom-host --reuse
incus stop loom-host
incus rebuild loom-host loom-host
incus start loom-host
```

The same steps update `loom-oracle`. The proxy devices, `boot.autostart`, and the volumes are part of the container configuration, so they stay.

The image of `loom-host` contains the agent of new codespaces. When the protocol version changes, update the oracle, the `loom-host` containers, and the clients together. Codespaces that exist keep their agent.
