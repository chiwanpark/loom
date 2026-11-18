# Remote hosts and relays

The client connects to agents in three ways:

- On your own machine (`local`).
- Over SSH.
- Through a relay.

## SSH hosts

To add an SSH host, use **Add an SSH host** in the Sessions panel. Or add an entry to `~/.config/loom/client.yaml`:

```yaml
hosts:
- kind: local
  name: local
- kind: ssh
  name: devbox
  destination: me@devbox.example.com
  remote_bin: /home/me/.cargo/bin/loom-agent   # optional; defaults to `loom-agent` on PATH
```

The client runs these commands on the host:

- `ssh <destination> loom-agent ps --json` to list the agents.
- `loom-agent stdio …` to connect to an agent.

The client uses one SSH connection for each host.

Non-interactive SSH sessions often do not have `~/.cargo/bin` or `~/.local/bin` on `PATH`. If the client cannot find `loom-agent`, set `remote_bin`.

## Relays

A relay lets clients connect to agents that do not accept SSH connections. The client and the agent both connect out to the relay. The traffic between the client and the agent has end-to-end encryption.

### Set up a relay

1. Start `loom-relay serve` on a host that the client and the agent can reach. `loom-relay config init` writes a configuration file. To install the relay as a system service, use `sudo scripts/install.sh relay --systemd`. The configuration of the service is `/etc/loom/relay.yaml`.
2. On each agent host, run `loom-agent host`. It shows the `host_id` of the host.
3. In `relay.yaml`, add each `host_id` to `allowed_hosts`. Then run `systemctl restart loom-relay`.
4. In `agent.yaml` of the agent, set `relay.enabled: true` and `relay.url`.

### Pair a client

1. On the agent host, run `loom-agent pair`. It shows a one-time `loom://` link and a QR code. By default, the link expires after 10 minutes.
2. In the desktop client, use **Pair through a relay** and paste the link.
3. In the Android app, use **Pair with a host**. Then scan the QR code or paste the link.

To manage the paired clients:

- `loom-agent clients list` shows the paired clients.
- `loom-agent clients revoke <label>` removes a client.

### Notifications for the Android app

The Android app gets notifications through Firebase Cloud Messaging. For this, the relay needs the private key of a service account of the Firebase project of the app.

1. In the Firebase console, open **Project settings** and then **Service accounts**.
2. Click **Generate new private key**. The language of the code sample is not important.
3. Put the JSON file on the relay host. Only the relay must be able to read it.
4. Add the file to `relay.yaml`:

```yaml
push:
  fcm:
    service_account: /etc/loom/firebase-service-account.json
```

The relay system service cannot read files that only root can read. `contrib/systemd/loom-relay.service` shows how to give the key to the service as a credential.

Agents send notifications only through a relay of this version or newer. An older relay accepts the connections of agents, but it does not send notifications. After you upgrade and restart the relay, the agents connect again and send notifications. You do not have to restart the agents.

The relay has these metrics for notifications:

| Metric | Meaning |
| --- | --- |
| `loom_relay_pushes_sent_total` | Notifications that the relay sent. |
| `loom_relay_pushes_rejected_total` | Notifications to tokens that Firebase does not know. The agents then forget these tokens. |

Other metrics count the notifications that the relay could not send and the notifications that it dropped.

## Keep an agent running

An agent for a relay must run all the time. `contrib/systemd/loom-agent.service` is a systemd user service. It runs `loom-agent serve --default`, which serves the `lifecycle.default_workspace` directory.

- `scripts/install.sh agent --systemd` installs and starts the service. The file `contrib/systemd/loom-agent.service` also has the manual steps.
- To keep the service running after you log out, run `loginctl enable-linger`.

When systemd runs the agent, `~/.config/loom/agent.env` sets variables in the environment of the agent. See [Environment variables and secrets](environment.md).
