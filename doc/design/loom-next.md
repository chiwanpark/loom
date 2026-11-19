# Loom-Next plan

Status: draft for discussion (2026-11-19, revision 5). Nothing in this document is final.

## 1. Goal

Loom-Next is a self-hosted system like GitHub Codespaces.

- A workspace is a Git repository.
- When you open a codespace from a workspace, the system makes a temporary container with a clone of the repository.
- The agent runs in the container. It can have more than one session.
- When you close the codespace, the system removes the container.
- The current Loom continues to work while we build Loom-Next. After Loom-Next is stable, we remove the classic modes.

## 2. Decisions

| Topic | Decision |
| --- | --- |
| Words | A codespace is a container with a clone and its sessions. A session is one thread with the model, as today. |
| Network | Some hosts are in a company network. The oracle cannot connect to them. They can connect out to the oracle. |
| Incus access | A `loom-host` daemon operates Incus for the oracle. The oracle holds no Incus certificate. |
| IncusOS | Some hosts run IncusOS, which has no shell. Thus, `loom-host` must also work without an installation on the host. |
| Storage | The IncusOS hosts use ZFS. The other hosts use the `dir` driver on ext4 or XFS. They cannot get a Btrfs or ZFS pool. |
| Users | One person uses an oracle. Paired devices have full access. |
| Close | The first version has only "delete with a guard". Stop and resume come later. |
| Environment | A base image and a setup script define the environment. Each host has its own environment configuration. |
| Docker | Docker must work in the container. |
| Connection types | The oracle and the client select the connection type for a codespace: relay or direct SSH. |
| GitHub Enterprise Server | Version 3.18.12. The oracle cannot reach it. The oracle can store its credential. No GitHub App in the organization, so it uses personal access tokens. |
| First message | The request that creates a codespace can contain the first message. The work starts without a connected client. |
| Lists | Each agent sends the title and the state of its sessions to the oracle. A client shows all lists from one connection. |
| Configuration | You do all configuration, also the model sign-in, in the desktop client. The Android app has no configuration functions. |
| Classic modes | Local hosts, SSH hosts, relay hosts for directories, worktree sessions, and the pairing with agents go away after Loom-Next is stable. |

## 3. Research: GitHub Codespaces

| GitHub Codespaces | Loom-Next |
| --- | --- |
| Each codespace is a VM with a Docker dev container in it. | Each codespace is an LXC system container on your Incus host. |
| The repository is cloned into `/workspaces`. The first clone is shallow. | The same directory. A shallow clone is an option. |
| `devcontainer.json`, features, and lifecycle commands define the environment. | A base image and a setup script define it. A subset of `devcontainer.json` is a later option. |
| Prebuilds keep the result of the setup, so a new codespace starts fast. | Workspace templates on hosts with ZFS. Prepared instances on hosts with `dir`. |
| `GITHUB_TOKEN` is limited to the repository. A Git credential helper uses it. | The oracle gives a token through a Git credential helper. |
| Secrets have a scope: user, repository, or organization. They become environment variables. | Secrets in the oracle have a scope: global, host, Git provider, or workspace. They go into the env file of the agent. |
| A dotfiles repository personalizes each codespace. | The agent configuration bundle does this. A dotfiles repository is a later option. |
| An idle codespace stops after 30 minutes (default). A stopped codespace is deleted after 30 days (default). | The same model, in a later phase. "Idle" means: no client, no running turn, no terminal activity. |
| Delete shows a warning for unpushed changes. You can export the changes to a branch. | A close guard does the same. It can push the branch of the codespace. |
| Forwarded ports are private, organization, or public. | The tunnels of the agent stay as they are. They are private. |
| Machine types set CPU, memory, and disk. | Size profiles set the Incus limits. |

Three ideas from GitHub Codespaces are the most important for this plan:

1. The environment holds no long-lived credential, where the provider permits this. It gets tokens from a central service.
2. Stop and delete are different operations. Stop keeps the disk. Delete has a guard against lost work.
3. Prebuilds make the start fast. Without them, each start pays for the clone and the setup.

Coder is the nearest open-source design. It has a control plane with a relay, and an agent in each workspace that connects back with a token. Its Git credentials also come from the control plane, with one provider for each Git host.

## 4. Parts of the current Loom that Loom-Next uses again

| Part | Use in Loom-Next |
| --- | --- |
| `loom-relay` (library and binary) | The oracle contains the relay. The static `allowed_hosts` list becomes a trait. |
| Noise handshake and `loom://` pairing (`loom-crypto`, `loom-transport`) | Devices, hosts, and containers pair with the oracle. |
| `loom-agent` and the agent protocol | The same agent runs in the container. Clients use the same protocol for sessions, terminals, files, and tunnels. |
| `loom-llm` `Auth` and `CredentialStore` | The oracle runs the sign-in and the token refresh with this code. |
| `loom-config` validation and JSON Schema | The oracle validates the configuration that the desktop client saves. |
| `loom-update` | `loom-host` gets `loom-agent` release binaries and checks their checksums. |
| Configuration hot reload | The agent in the container applies configuration changes from the oracle without a restart. |
| SSH transport (`loom-agent stdio`) | The direct SSH connection type uses the same stream. |

The worktree code is not a base for Loom-Next, because it goes away. The Git actions of a codespace (status for the guard, push, pull request) are new code.

## 5. Architecture

```
 desktop / Android ──(1) control service ──▶ ┌──────────── loom-oracle ────────────┐
        │                                    │ control service │ relay             │
        ├──(2) relay, end-to-end encrypted ─▶│ SQLite: devices, hosts, providers,  │
        │                                    │ workspaces, codespaces,             │
        │                                    │ configuration, secrets              │
        │                                    └──────▲────────────────▲─────────────┘
        │                         (3) commands and  │                │ (4) bootstrap, tokens,
        │                             events        │                │     relay registration
        │                                   ┌───────┴──── host ──────┴───────┐
        └──(5) SSH, where it is possible ──▶│ loom-host ──▶ Incus API        │
                                            │ container = codespace          │──▶ GitHub, GHES,
                                            │ loom-agent, clone, Docker      │    model providers
                                            └────────────────────────────────┘
```

The hosts and the containers start all their connections to the oracle. The oracle never connects to a host.

### Terms

| Term | Meaning |
| --- | --- |
| Oracle | The public daemon: control service, relay, and store. |
| Host | An Incus server that a `loom-host` daemon operates. |
| Git provider | One GitHub instance: github.com or a GitHub Enterprise Server. |
| Workspace | A repository on a Git provider. |
| Codespace | A container with a clone of the workspace, one agent, and one or more sessions. |
| Session | One thread with the model. The sessions of a codespace use the same clone. The clients and the code use this word today. |
| Device | A desktop or Android client that is paired with the oracle. |

### Components

| Component | Status | Contents |
| --- | --- | --- |
| `loom-oracle` | New crate | Control service, embedded relay, store, codespace state machine, Git provider clients, credential broker. |
| `loom-host` | New crate | The daemon for each host. It connects out to the oracle and operates the Incus server. |
| `loom-incus` | New crate | A small client for the Incus REST API. It uses the local socket, or HTTPS with a client certificate. |
| `loom-oracle-proto` | New crate | The messages of the control service. |
| `loom-relay` | Changed | The host check becomes a trait. Limits apply to each registered peer, not only to each IP address. The binary and `relay.yaml` do not change. |
| `loom-agent` | Changed | New managed mode for containers. New `git-credential` command. |
| `loom-llm` | Changed | The credential source becomes a trait: local file or oracle. |
| `loom-client`, `loom-gui`, `loom-mobile`, Android | Changed | See section 14. |

### Codespace start

1. The client sends `CreateCodespace` with the workspace, the base branch, the host or "automatic", the size, and the client version. It can also send the first message with its model.
2. The oracle selects a host that can reach the Git provider and has capacity. It writes the codespace row and makes a one-time bootstrap secret.
3. The oracle sends the command to `loom-host`. `loom-host` creates the instance from the image of the host.
4. `loom-host` puts in the `loom-agent` binary of the client version, the systemd unit, and the bootstrap file. Then it starts the instance.
5. In the container, systemd starts `loom-agent serve --managed`.
6. The agent makes its own keys. It pairs with the oracle with the bootstrap secret. The secret works one time.
7. The agent gets its bundle: configuration, instructions, skills, secrets, Git identity, and device keys.
8. The agent clones the repository, makes the branch `loom/<codespace>`, and runs the setup script.
9. The agent registers with the relay and reports `ready`. The oracle sends the progress of steps 3 to 9 to the client.
10. If there is a first message, the agent starts a session with it. No client is necessary for this step.
11. The client connects to the agent with the selected connection type when you open a session.

### Codespace close

1. The client asks the agent for uncommitted changes and unpushed commits.
2. If there is such work, you choose: push the branch of the codespace, or discard the work.
3. The oracle tells `loom-host` to delete the instance. It revokes the keys and tokens of the codespace.

## 6. Changes to the requirements

You agreed to 6.3, 6.6, and 6.7. The other points had no objection until now.

### 6.1 The oracle contains the relay (requirement 1)

One daemon and one URL do the work of `loom-relay` and the new control service. Current agents can use the oracle as their relay without a change. Thus, the current Loom works with the old relay or with the oracle.

### 6.2 The control service uses the relay channel (requirement 1)

The control service registers on its own relay as an agent with the instance name `oracle`. Devices, hosts, and containers then use the code that clients use for agents today: `loom://` pairing link, QR code, Noise handshake, and Cloudflare Access. Each paired peer has a role: device, host, or codespace.

The messages are JSON, not postcard. Receivers ignore unknown fields. Thus, the oracle, old hosts, old containers, and clients of different versions can talk. The exact-version rule of the agent protocol does not go into the control service.

### 6.3 `loom-host` operates Incus (requirements 2 and 3)

The oracle cannot connect to hosts in the company network. Thus, a small daemon, `loom-host`, connects out to the oracle, as agents connect to the relay today. The oracle holds no Incus certificate.

- `loom-host` accepts only a small set of commands, such as "create a codespace instance" and "delete a codespace instance".
- `loom-host` gets the `loom-agent` binary and builds images near the host. No large data goes through the oracle.
- `loom-host` can clean up when the oracle is down.
- `loom-host` can apply local rules that the oracle cannot change.

`loom-host` does all its work through the Incus API. It needs no shell on the host. Thus, it also works for IncusOS. See section 7.2.

### 6.4 The oracle becomes a trusted service (requirements 3, 5, 6)

Today the relay cannot read anything. The oracle holds the GitHub credentials and the model credentials. It also decides which code starts on each host. This is a real change of the security model.

- The oracle encrypts secrets in SQLite with a master key from a file or a systemd credential.
- Only paired devices can use the control service.
- The traffic between a client and an agent stays end-to-end encrypted. A proxy such as Cloudflare cannot read it.
- Codespaces run as unprivileged containers in a restricted Incus project.
- Each `loom-host` has local rules in its configuration file: the permitted Git providers, the maximum number of codespaces, and the permitted images.

### 6.5 Containers get credentials from a broker, not copies (requirements 3, 4, 6)

A copy of an OAuth credential in each container breaks when a provider rotates the refresh token. One container refreshes, and the copies in the other containers become invalid. `loom-llm` already stores a new refresh token when a model provider returns one.

- The oracle keeps the refresh tokens and is the only service that refreshes.
- For models, the agent asks the oracle for a current access token. Sign-in occurs one time at the oracle, not in each container.
- For Git, a credential helper (`loom-agent git-credential`) asks the oracle for the token. The token is not in the image and not in a file.

A classic agent and the oracle cannot share one sign-in for the same reason. Sign in again at the oracle.

### 6.6 Calls to a Git provider can go through a host (requirements 3 and 4)

The oracle cannot reach your GitHub Enterprise Server. The oracle keeps the credential, and it sends each API call for this provider through a `loom-host` that can reach the server. Examples are the search of repositories and the list of branches. `loom-host` sends such calls only to the API URL of a registered provider.

### 6.7 Stop is different from delete

Agents work when no client is connected. Thus, "close" must be an explicit action, not a disconnect. The first version has close with the guard of section 5. Stop on idle and delete after a retention period come in phase 6.

### 6.8 Data that must outlive a container goes to the oracle

Today the notes of an agent stay when you delete a session. In a temporary container, they are lost. The oracle keeps the notes for each workspace, and each new codespace gets them. An archive of sessions in the oracle is a later option.

### 6.9 The container is the sandbox

In managed mode, the default policy is `allow`, and the landlock or bubblewrap sandbox is off. The container and the limited tokens set the limit of the damage.

### 6.10 The agent version follows the client

The agent protocol needs the same version on both sides. `loom-host` keeps a cache of `loom-agent` releases and installs the version of the client that creates the codespace. The current update command covers later client updates.

## 7. Design details

### 7.1 Store

SQLite with `rusqlite`, which the agent already uses. Numbered migrations.

| Table | Contents |
| --- | --- |
| `peers`, `pairings` | Paired devices, hosts, and codespace agents with their keys and roles. Push tokens of devices. One-time pairing secrets. |
| `relay_hosts` | Host IDs of classic agents. Replaces `allowed_hosts`. Phase 8 removes it. |
| `hosts` | Name, capacity, storage driver, reachable providers, connection types, environment configuration, size profiles. |
| `providers` | Name, web URL, API URL, Git identity, route (direct or through hosts). |
| `provider_credentials` | Tokens (encrypted) with a scope: provider, owner, or workspace. Expiry date. |
| `workspaces` | Provider, owner, repository, default host, default size, setup options. |
| `codespaces` | Workspace, host, instance name, state, branch, keys of the agent, times. The first message until the agent takes it. |
| `sessions` | The last summary of each session: title, state, time. |
| `config_files`, `secrets` | The configuration bundle and the secrets, with a scope. |
| `llm_credentials` | The credential file of `loom-llm`, encrypted. |
| `notes` | Notes for each workspace. |
| `audit` | What each peer did and when. |

Daemon settings, such as the listen address and TLS, stay in a YAML file (`oracle.yaml`) like `relay.yaml`.

### 7.2 Hosts and `loom-host`

| Host type | Where `loom-host` runs | How it reaches Incus |
| --- | --- | --- |
| Linux with a shell | On the host, as a systemd service. | The local Incus socket. |
| IncusOS | In an instance on the host, outside the project of the codespaces. A different machine in the same network is also possible. | The HTTPS API with a restricted client certificate for the project of the codespaces. |

- All operations use the Incus API: image build, instance, files, and exec. No operation needs a shell on the host.
- The certificate stays with `loom-host`. The oracle does not get it.
- One `loom-host` operates one Incus server.
- The desktop client adds a host and shows a one-time `loom://` link. `loom-host join <link>` pairs with the oracle. Then `loom-host serve` runs as a service.
- With the local socket, `loom-host` makes the restricted Incus project. With a certificate, it cannot. Then you make the project and the trust token with the `incus` CLI from your machine.
- `loom-host check` tests the project settings, the storage driver, and Docker in a test instance.
- `loom-host` reports the CPUs, the memory, the storage driver, the reachable Git providers, and the connection types.
- The commands from the oracle are: build the image, create, start, stop, delete, and get the status of a codespace instance.
- A driver trait has two implementations. `IncusDriver` uses `loom-incus`. `ProcessDriver` starts a local `loom-agent` process, for tests and for development without Incus.
- Each instance has `user.loom.*` keys that name the oracle and the codespace. `loom-host` reports its instances, and the oracle compares them with the `codespaces` table. Orphans are deleted, and lost codespaces get a mark.
- A codespace gets CPU, memory, and disk limits from its size profile.
- A later option is an OCI image of `loom-host`. Then one `incus launch` command installs it on IncusOS.

### 7.3 Image, Docker, and the environment of a host

- The oracle keeps the environment configuration of each host (requirement 3). It has: the base image, a host setup script, environment variables such as proxy settings, CA certificates, and Docker settings such as registry mirrors.
- `loom-host` builds the codespace image through the Incus API. It starts an instance from the base image, runs the base script of Loom, and then runs the host setup script. Then it publishes the instance as an image.
- The base script installs git, `gh`, Docker Engine, and a user with `sudo`.
- `loom-host` builds the image again when the environment configuration of the host changes.
- For Docker, each instance has `security.nesting=true`. Some image layers also need the syscall interception of Incus for `mknod` and `setxattr`.
- The restricted project must permit both: `restricted.containers.nesting=allow` and `restricted.containers.interception=allow`.

The storage driver of the Incus pool changes the behavior:

| Topic | ZFS (IncusOS hosts) | `dir` on ext4 or XFS (other hosts) |
| --- | --- | --- |
| New instance from the image | A copy-on-write clone. Fast. | A full copy of the image. Slower. |
| Disk limit of a codespace | Yes. | Only if the file system has project quotas on. |
| Docker storage | Overlay on ZFS needs OpenZFS 2.2 or later. The fallback is a block-mode volume for `/var/lib/docker`. | `overlay2` works on ext4, and on XFS with `ftype=1`. |
| Fast start in phase 7 | Workspace templates. | A small number of prepared instances. |

On IncusOS, you cannot load kernel modules by hand. Phase 0 tests Docker on one host of each type.

### 7.4 Bootstrap

- `loom-host` puts three files into a new instance before the start: the `loom-agent` binary, the systemd unit, and the bootstrap file.
- The bootstrap file has the oracle URL, the oracle key, the codespace ID, and the one-time secret. The agent deletes it after use.
- The agent runs as a normal user with `sudo`.

### 7.5 Git providers (requirement 4)

A provider has a web URL and an API URL. Examples are `https://github.com` with `https://api.github.com`, and `https://ghe.example.com` with `https://ghe.example.com/api/v3`.

The first authentication method is a personal access token, because you do not install a GitHub App in the organization.

| Token type | Reach | Notes |
| --- | --- | --- |
| Fine-grained personal access token | One owner (one organization or your account). All its repositories or the repositories that you select. | Recommended. Your server version 3.18 has it. The organization can require an approval for each token. |
| Classic personal access token | All your repositories. | Works on all instances. |

- A credential has a scope: provider, owner, or workspace. The oracle uses the credential with the smallest scope that matches. Thus, you can use one token for an organization, or a token for only one repository.
- A fine-grained token needs these repository permissions: Contents (write), Pull requests (write), and Metadata (read). Add others for the `gh` commands that you use.
- The oracle reads the expiry date of each token from the API responses. The clients show a warning before a token expires.
- A token is long-lived, and each program in the container can ask the helper for it. A fine-grained token with selected repositories limits the damage. The Git proxy of phase 7 removes the token from the container.
- The credential helper is set for each Git host. A codespace gets credentials only for the provider of its workspace.
- The agent writes the token into the configuration of `gh`, so `gh` and **Open pull request** work.
- Each provider has its own Git name and email.

A GitHub App is a later method. Its tokens are for one repository and expire in 1 hour. But an app reaches only the accounts and organizations where it is installed. This is also true for the user tokens of the app.

### 7.6 Network reach

- Each `loom-host` tests which Git providers it can reach and reports the result.
- A codespace runs only on a host that can reach the provider of its workspace.
- The oracle calls the API of a provider directly when it can. If it cannot, it sends the call through a `loom-host` that can.
- Thus, the Android app outside the company network can also show the repositories of your GitHub Enterprise Server.

### 7.7 Agent configuration (requirement 5)

| Layer | Examples |
| --- | --- |
| Global | `agent.yaml`, `AGENTS.md`, skills, secrets. |
| Host | Environment variables, CA certificates, Docker settings, secrets of the host. |
| Workspace | Secrets, default size, setup options. |
| Repository | `AGENTS.md`, skills, and the setup script `.loom/setup.sh` in the repository. |

- A later layer replaces the values of an earlier layer.
- The managed mode sets some sections itself: `relay`, `socket`, `storage`, `lifecycle`, and `allowed_roots`. It has no worktrees.
- When the bundle changes, the oracle tells the agents. They write the files, and the current hot reload applies them.

### 7.8 Connection types (requirement 1)

- Each host has a list of connection types. `relay` is always there. `ssh` is there when you set an SSH destination for the host.
- A client tells the oracle the types that it supports. The desktop client supports the relay and SSH. The Android app supports the relay.
- For each codespace, the oracle gives the types that the two sides support, in the order of preference, with their data.
- The oracle cannot know if a client can reach a host by SSH at this time. Thus, the client tries SSH with a short timeout and then uses the relay.
- The SSH type runs `ssh <destination> loom-host stdio --codespace <id>`. `loom-host` connects the stream to the agent in the container through the exec function of the Incus API.
- The SSH destination is the machine where `loom-host` runs. For IncusOS, this is the instance of `loom-host`, which then needs an SSH server and an address that the client can reach.
- As with SSH hosts today, the SSH type trusts the SSH account. The relay type uses the Noise keys.

### 7.9 Codespace states

`provisioning` → `starting` → `ready` → `closing` → `closed`. Phase 6 adds `stopped`. Error states are `failed` and `lost`.

When the oracle restarts, codespaces continue to run. The agents and the hosts connect again, as agents do with the relay today. Only token refresh and new codespaces wait for the oracle.

### 7.10 Lists and events

- Each agent sends a summary of a session to the oracle when it changes: title, state, time, and the archived mark.
- The oracle keeps the last summary. Thus, the lists also show a codespace that is not connected.
- A client subscribes to events on its connection to the oracle: codespace states, start progress, and session summaries.
- The oracle also gives the list of models, because a client needs it before a codespace is there.
- The content of sessions does not go to the oracle. The first message is the exception. The oracle keeps it until the agent takes it, and then deletes it.
- Notifications do not change: each agent encrypts them for the devices and sends them through the relay.

### 7.11 Configuration operations

- The control service has all configuration operations: devices, hosts, Git providers, credentials, model sign-in, the configuration bundle, secrets, and workspace settings.
- The desktop client is the user interface for them. See section 14.
- The oracle validates each change. It rejects a bad `agent.yaml` and gives the reason, as the agent does today.
- Secrets and tokens are write-only. A client can set, replace, and delete them, but it cannot read them.
- `loom-oracle` has only these commands: `serve`, `config` for the daemon file, and `pair` for the first device.
- A test CLI uses the same control service. It is for tests and for the phases before the screens are there.

## 8. Compatibility with the current Loom (requirement 7)

- All work is on `main`, in new crates and behind new entry points. There is no long-lived branch.
- The agent protocol does not change for Loom-Next, if possible. New commands go at the end of `Command`, which needs no version bump.
- `loom-relay`, `relay.yaml`, local hosts, SSH hosts, relay hosts, and worktree sessions continue to work until phase 8.
- The classic modes get fixes during this time, but no new functions.
- `client.yaml` gets a new host kind. Old kinds do not change until phase 8.
- The normal checks (fmt, clippy, tests) run for the old and the new code in each commit.
- Releases contain `loom-oracle` and `loom-host` as experimental binaries after phase 3.

## 9. Phases

Each phase ends with a state that you can use and test. Sizes are relative: S, M, L.

| Phase | Size | Work | Result |
| --- | --- | --- | --- |
| 0. Spike | S | By hand with the `incus` CLI on one IncusOS host and one Linux host: restricted project, container, Docker, current `loom-agent`, current relay. Measure the start time. Test the connections from the container to the relay, the GitHub instances, and the model providers. | Proof that an unchanged agent and Docker work in a container on the two host types. Real numbers for the start time. |
| 1. Oracle as relay | M | Host check trait and limits for each peer in `loom-relay`. `loom-oracle serve`, store, secret encryption. Pairing with roles. Control service with `Hello`, events, and the device operations. Client library and the test CLI. | The current agents and clients work with the oracle as their relay. A paired device can call the control service. |
| 2. Hosts and codespaces | L | `loom-host` with `join`, `check`, the link to the oracle, and `ProcessDriver`. `IncusDriver` on `loom-incus` with the socket and with a certificate. Image build with the environment of the host. Agent cache. Codespace state machine and instance comparison. Managed mode: bootstrap, keys, clone of a public repository, `ready`. | The test CLI makes a codespace on an IncusOS host and on a Linux host. The current desktop client connects to it with a relay host entry made by hand. |
| 3. Git providers | M | Providers and credentials with scopes. Calls through a host. Host selection by network reach. Repository search and branch list. Credential helper, Git identity, `gh`. Git actions of a codespace: status, push, pull request. Close guard. | Codespaces for private repositories on github.com and on your GitHub Enterprise Server can clone, push, and open a pull request. |
| 4. Configuration and model credentials | M | Bundle layers and live update. Credential source trait in `loom-llm`, broker, sign-in at the oracle. Notes for each workspace. Managed defaults for policy and sandbox. First message. Session summaries. | A new codespace needs no setup and no sign-in, and it starts its first turn without a client. Ten codespaces stay signed in for longer than one token lifetime. |
| 5a. Clients: daily work | L | Desktop and Android: host kind `oracle`, pairing, **Workspaces**, **New codespace** with the first message, progress view, lists from the oracle, codespace menu, notifications. A first version can start after phase 2. | You do all daily work from the desktop client and the Android app. |
| 5b. Desktop client: configuration | L | The configuration pages of section 14. | You do all configuration in the desktop client. |
| 6. Lifecycle and SSH | M | Stop and resume, idle timeout, retention, limits for each host, metrics, audit. The SSH connection type and its selection. | Codespaces clean up themselves. The desktop client uses SSH where it can. |
| 7. Speed and hardening | L | Workspace templates on ZFS, prepared instances on `dir`. Network rules for containers. A Git proxy that keeps tokens out of containers. GitHub App method. OCI image of `loom-host`. VM instances. `devcontainer.json` subset. Session archive. | A warm codespace starts in seconds. |
| 8. Removal of the classic modes | M | Remove local hosts, SSH hosts, relay hosts for directories, worktree sessions, and the pairing with agents. Remove the code that only they use. Clean up the agent protocol with one version bump. | Loom has one mode. |

### Phase 0: the first host

The first test host is an IncusOS host. The machine where we develop Loom is an unprivileged container on this host. Thus, this machine can have the role of the `loom-host` instance in section 7.2.

Facts that we measured in this container on 2026-11-19:

- The host has kernel 7.2 and OpenZFS 2.4.4. The root disk of the container is a ZFS dataset.
- Docker Engine 29.9 with the `overlayfs` storage driver starts containers here. No block-mode volume is necessary.
- The container can reach the Incus HTTPS API of the host at the address of the bridge.

The settings of this container are not known yet, for example nesting and syscall interception. Phase 0 must do the same test in a container of a restricted project. A Linux host with the `dir` driver comes after that.

## 10. Tests

- Unit tests for the store, the control service, and the codespace state machine.
- `wiremock` tests for the Incus API and the GitHub API. The repository already uses `wiremock`.
- End-to-end tests without Incus: the oracle, `loom-host` with `ProcessDriver`, a managed agent, and the client library in one test.
- Tests with a real Incus server are marked `#[ignore]`. They run by hand or in a separate CI job.
- The current relay tests run against `loom-relay` and against the relay in the oracle.

## 11. Risks

| Risk | Mitigation |
| --- | --- |
| The oracle is one point of failure and a valuable target. It can start code in the company network. | Encrypted secrets, local rules of each `loom-host`, restricted Incus project, Cloudflare Access, audit log. Codespaces continue when the oracle is down. |
| A long-lived token for the GitHub Enterprise Server is in the oracle, and containers can read it. | Fine-grained tokens with selected repositories and an expiry date. The Git proxy of phase 7. |
| All containers of the company network have one public IP address. The relay limits connections and messages for each IP address. | Limits for each registered peer in phase 1. |
| Docker does not work in a container on IncusOS, for example because of kernel modules or overlay on ZFS. | Phase 0 tests it first. The fallbacks are a block-mode volume and VM instances. |
| Hosts with the `dir` driver start codespaces slowly and possibly have no disk limits. | Measure in phase 0. A small image. Prepared instances in phase 7. |
| Version skew: old containers, a new client, and two clients of different versions. | The agent version follows the client. The current update command. JSON in the control service. |
| Lost work when a container is deleted. | Close guard, push of the branch. Stop and retention warnings in phase 6. |
| A container can reach the network of its host. | Network rules in phase 7. A note in the host setup document before that. |
| Docker in the container needs nesting, which makes the isolation weaker. | Unprivileged containers in a restricted project. VM instances as an option in phase 7. |
| Disk use: each codespace has a full clone, its build output, and Docker images. | Disk limits, retention, registry mirrors in the Docker settings of the host. |
| The configuration pages are a large part of the desktop work. | Text editors with validation for files. Forms only for lists. The test CLI until the pages are there. |
| After phase 8, all work needs an oracle and an Incus host. | `ProcessDriver` stays for the development of Loom. |

## 12. Not in this plan

- A web client.
- Configuration functions in the Android app.
- Full support of `devcontainer.json` with Docker features.
- Public URLs for forwarded ports.
- More than one user with separate permissions.
- A move of old sessions from classic agents into Loom-Next.

## 13. Open questions

There are no open questions at this time.

## 14. Clients

### Structure

Today the structure is host → agent → sessions. The new structure is oracle → workspace → codespace → sessions. A codespace takes the place of the agent. Thus, the session view, terminals, files, changes, ports, notes, and tasks do not change.

- `client.yaml` gets the host kind `oracle`. You pair with the current `loom://` dialog or QR code.
- The groups by state stay: Needs approval, Working, Completed, Idle, Archived. Each row is a session. It shows the workspace and the branch of its codespace.
- **Workspaces** takes the place of **Remotes**. It shows each workspace with its codespaces. For each codespace, it shows the branch, the host, the number of sessions, and the status.
- The lists come from the oracle. See section 7.10. A client connects to an agent only when you open a session.
- While the classic modes are there, the panel shows **Remotes** and **Workspaces**. Phase 8 removes **Remotes**.

### New codespace

1. `ctrl-n` or **+** opens a list of workspaces: the repositories that you used, and a search on the Git providers.
2. A dialog asks for the base branch, the host, and the size. The host is "automatic" by default.
3. You can type the first message and select its model immediately. The oracle keeps the message and gives it to the agent when the codespace is ready.
4. A progress view shows the steps of the start, and the log of the setup script.

Thus, you can start a task on the phone, close the app, and get a notification when the turn is complete.

### Codespace menu

- **New session** adds a session in the same clone.
- **Open pull request** pushes the branch of the codespace and opens a pull request.
- **Close codespace** shows the guard and then removes the container.
- **Merge worktree** has no successor. A codespace ends with a push or a pull request.

### Configuration in the desktop client

Each oracle has a **Settings** tab with these pages:

| Page | Contents |
| --- | --- |
| Devices | The paired devices. Add a device with a link and a QR code. Revoke a device. |
| Hosts | The hosts with their status and the results of `loom-host check`. Add a host with a join link. Environment configuration, size profiles, and SSH destination of each host. |
| Git providers | URLs, Git name and email. Tokens with their scope and expiry date. |
| Models | **Providers and sign-in…** for the oracle, and the default model. |
| Agent configuration | `agent.yaml`, `AGENTS.md`, skills, and secrets for each layer: global, host, workspace. |
| Workspaces | Default host, default size, and setup options of each workspace. |

- Files such as `agent.yaml`, `AGENTS.md`, and the setup scripts open in the text editor of the file view. The oracle validates a file when you save it.
- Lists such as devices, hosts, tokens, and secrets use forms.
- **Import from this machine** reads `~/.config/loom` and sends your current configuration to the oracle.
- The first device is the exception. `loom-oracle pair` on the oracle machine shows its link. After that, the desktop client pairs the other devices.
- The local rules of a `loom-host` are not in the client. They stay in the file on the host, so that the oracle cannot change them.

### Android

- The same structure as the desktop client for daily work.
- No configuration pages. The app shows warnings, such as a token that expires soon.
- The app registers its push token with the oracle. Each agent gets the token and the key of the device in its bundle.
- The app gets the keys of each codespace from the oracle, so it can open their notifications.

## Sources

- GitHub Codespaces: [deep dive](https://docs.github.com/en/codespaces/about-codespaces/deep-dive), [lifecycle](https://docs.github.com/en/codespaces/about-codespaces/understanding-the-codespace-lifecycle), [repository access](https://docs.github.com/en/codespaces/managing-your-codespaces/managing-repository-access-for-your-codespaces), [secrets](https://docs.github.com/en/codespaces/managing-your-codespaces/managing-development-environment-secrets), [prebuilds](https://docs.github.com/en/codespaces/prebuilding-your-codespaces), [export to a branch](https://docs.github.com/en/codespaces/troubleshooting/exporting-changes-to-a-branch).
- GitHub tokens: [personal access tokens](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens), [endpoints for fine-grained tokens on 3.18](https://docs.github.com/en/enterprise-server@3.18/rest/authentication/endpoints-available-for-fine-grained-personal-access-tokens), [token policy of an organization](https://docs.github.com/en/enterprise-server@3.20/organizations/managing-programmatic-access-to-your-organization/setting-a-personal-access-token-policy-for-your-organization), [expiry header](https://github.blog/changelog/2021-07-26-expiration-options-for-personal-access-tokens/).
- GitHub Apps: [installation tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app), [user tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-with-a-github-app-on-behalf-of-a-user), [installation requests](https://docs.github.com/en/apps/using-github-apps/requesting-a-github-app-from-your-organization-owner).
- Incus: [REST API](https://linuxcontainers.org/incus/docs/main/rest-api-spec/), [authentication](https://linuxcontainers.org/incus/docs/main/authentication/), [projects](https://linuxcontainers.org/incus/docs/main/reference/projects/), [syscall interception](https://linuxcontainers.org/incus/docs/main/syscall-interception/), [Docker in a container](https://linuxcontainers.org/incus/docs/main/faq/), [storage drivers](https://linuxcontainers.org/incus/docs/main/reference/storage_drivers/), [ZFS driver](https://linuxcontainers.org/incus/docs/main/reference/storage_zfs/).
- IncusOS: [introduction](https://linuxcontainers.org/incus-os/introduction/), [Incus application](https://linuxcontainers.org/incus-os/docs/main/reference/applications/incus/), [storage](https://linuxcontainers.org/incus-os/docs/main/reference/system/storage/).
- Coder: [architecture](https://coder.com/docs/install/plan/architecture/), [external authentication](https://coder.com/docs/admin/external-auth).
