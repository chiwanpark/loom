# Loom-Next plan

Status: draft for discussion (2026-11-22, revision 22). Nothing in this document is final.

## 1. Goal

Loom-Next is a self-hosted system like GitHub Codespaces.

- A workspace is a collection of Git repositories and mounted volumes. You also set the host for each workspace.
- An agent configuration has the settings, the instructions, the skills, and the credentials of an agent. A preset is an agent configuration with a name, which the oracle keeps.
- A codespace is a temporary container on the host of its workspace. It runs one agent with one preset on one workspace.
- The container has a clone of each repository. The agent can have more than one session.
- When you close the codespace, the system removes the container. The volumes stay.
- The current Loom continues to work while we build Loom-Next. After Loom-Next is stable, we remove the classic modes.

## 2. Decisions

| Topic | Decision |
| --- | --- |
| Words | An agent configuration is a YAML file, `AGENTS.md`, skills, and credentials from the oracle. A preset is an agent configuration with a name. A workspace is a collection of repositories and mounted volumes, with a host. A codespace is a container on that host with one preset and one workspace. A session is one thread with the model, as today. |
| Network | Some hosts are in a company network. The oracle cannot connect to them. They can connect out to the oracle. |
| Incus access | A `loom-host` daemon operates Incus for the oracle. The oracle holds no Incus certificate. |
| IncusOS | Some hosts run IncusOS, which has no shell. Thus, `loom-host` must also work without an installation on the host. |
| Storage | The IncusOS hosts use ZFS. The other hosts use the `dir` driver on ext4 or XFS. They cannot get a Btrfs or ZFS pool. |
| Linux host | Your Linux host has an Enterprise Linux 9 distribution, kernel 5.14, an XFS root file system, and Incus 7.0.2. SELinux is disabled on it, and firewalld is not active. You confirmed this with `getenforce` and `systemctl is-active firewalld`. |
| Users | One person uses an oracle. Paired devices have full access. |
| Close | The first version has only "delete with a guard". Stop and resume come later. |
| Environment | A base image and a setup script define the environment. Each host has its own environment configuration. |
| Docker | Docker must work in the container. |
| Connection types | The oracle and the client select the connection type for a codespace: relay or direct SSH. |
| GitHub Enterprise Server | Version 3.18.12. The oracle cannot reach it. The oracle can store its credential. No GitHub App in the organization, so it uses personal access tokens. A credential for tests is not available now. The plan assumes that the server works as github.com does, and phase 3 tests it. |
| Git providers of a codespace | A codespace can use more than one Git provider, for example github.com and your GitHub Enterprise Server. It gets the Git credentials that its preset names. |
| Repositories of a workspace | A workspace can have more than one repository, for work that changes them together. A codespace has a clone of each repository. |
| Volumes of a workspace | A workspace can have mounted volumes. A volume is storage on a host that stays when a codespace closes. A volume is on one host, and the oracle does not copy it. A volume has no size limit, unless you set one. |
| Host directories as volumes | A volume can also be a directory of the host. Your Linux host mounts an NFS server, and some of its directories are volumes. Each NFS directory has one owner, and this user is the same on all hosts. Codespaces also write to the directories. One host user for all codespaces of a host is sufficient. |
| Host of a workspace | You set the repositories and the host for each workspace. All codespaces of a workspace run on its host. The oracle does not select a host automatically. |
| Resource limits | You set the CPU limit and the memory limit of each codespace in **New codespace**. The default values are 2 CPUs and 4 GiB of memory. There are no size profiles, and a codespace has no disk limit. |
| First message | The request that creates a codespace can contain the first message. The work starts without a connected client. |
| Lists | Each agent sends the title and the state of its sessions to the oracle. A client shows all lists from one connection. |
| Configuration | You do all configuration, also the model sign-in, in the desktop client. The Android app has no configuration functions. |
| Model accounts | You have more than one account for a model provider, for example a personal and a company Claude plan. The oracle keeps all accounts. A codespace uses one account for each model provider. |
| Presets | The oracle keeps presets of agent configuration. Each preset has a name, one YAML file, `AGENTS.md`, and skills. The YAML file names its credentials in the oracle: model accounts, Git credentials, and secrets. It also sets the name of the environment variable for each secret. Each codespace uses one preset and keeps it until it closes. |
| Classic modes | Local hosts, SSH hosts, relay hosts for directories, worktree sessions, and the pairing with agents go away after Loom-Next is stable. |
| Branch | The development branch of Loom-Next is `loom-next`. No Loom-Next branch merges into `main` until the transition is finished, which is after phase 8. There are no pre-releases from `loom-next` unless you ask for them. |

## 3. Research: GitHub Codespaces

| GitHub Codespaces | Loom-Next |
| --- | --- |
| Each codespace is a VM with a Docker dev container in it. | Each codespace is an LXC system container on your Incus host. |
| The repository is cloned into `/workspaces`. The first clone is shallow. | The same directory. A shallow clone is an option. |
| A codespace is for one repository. You give access to other repositories in `devcontainer.json` and clone them by hand. | A workspace can have more than one repository. The codespace clones all of them. |
| `devcontainer.json`, features, and lifecycle commands define the environment. | A base image and a setup script define it. A subset of `devcontainer.json` is a later option. |
| Prebuilds keep the result of the setup, so a new codespace starts fast. | Each codespace starts from a fresh instance. There are no prebuilds (section 12). |
| `GITHUB_TOKEN` is limited to the repository. A Git credential helper uses it. | The oracle gives a token through a Git credential helper. |
| Secrets have a scope: user, repository, or organization. They become environment variables. | A preset names the secrets that its codespaces get, and the environment variable of each secret. |
| A dotfiles repository personalizes each codespace. | The preset does this. A dotfiles repository is a later option. |
| An idle codespace stops after 30 minutes (default). A stopped codespace is deleted after 30 days (default). | The same model, in a later phase. "Idle" means: no client, no running turn, no terminal activity. |
| Delete shows a warning for unpushed changes. You can export the changes to a branch. | A close guard does the same. It can push the branch of the codespace. |
| Forwarded ports are private, organization, or public. | The tunnels of the agent stay as they are. They are private. |
| Machine types set CPU, memory, and disk. | **New codespace** sets the CPU limit and the memory limit of each codespace. There is no disk limit. |

Three ideas from GitHub Codespaces are the most important for this plan:

1. The environment holds no long-lived credential, where the provider permits this. It gets tokens from a central service.
2. Stop and delete are different operations. Stop keeps the disk. Delete has a guard against lost work.
3. Prebuilds make the start fast. Without them, each start pays for the clone and the setup. This plan accepts this cost and uses fresh instances.

Coder is the nearest open-source design. It has a control plane with a relay, and an agent in each workspace that connects back with a token. Its Git credentials also come from the control plane, with one provider for each Git host.

## 4. Parts of the current Loom that Loom-Next uses again

| Part | Use in Loom-Next |
| --- | --- |
| `loom-relay` (library and binary) | The oracle contains the relay. The static `allowed_hosts` list becomes a trait. |
| Noise handshake and `loom://` pairing (`loom-crypto`, `loom-transport`) | Devices, hosts, and containers pair with the oracle. |
| `loom-agent` and the agent protocol | The same agent runs in the container. Clients use the same protocol for sessions, terminals, files, and tunnels. |
| `loom-llm` `Auth` and `CredentialStore` | The oracle runs the sign-in and the token refresh with this code. It has one credential store for each model account. |
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
                                            │ loom-agent, clones, Docker     │    model providers
                                            │ volumes of the workspaces      │
                                            └────────────────────────────────┘
```

The hosts and the containers start all their connections to the oracle. The oracle never connects to a host.

### Terms

| Term | Meaning |
| --- | --- |
| Oracle | The public daemon: control service, relay, and store. |
| Host | An Incus server that a `loom-host` daemon operates. |
| Git provider | One GitHub instance: github.com or a GitHub Enterprise Server. |
| Git credential | A token for a Git provider, with a name and a scope. |
| Model provider | A service that gives models, as today: `anthropic`, `openai`, `openai-codex`, or an OpenAI-compatible server. |
| Model account | One sign-in or one API key for a model provider. A model provider can have more than one account. |
| Secret | A name and a value that the oracle keeps, for example an API key for a tool. A preset gives it the name of an environment variable. |
| Agent configuration | One YAML file, `AGENTS.md`, and skills. The YAML file names the credentials that the oracle gives: model accounts, Git credentials, and secrets. |
| Preset | An agent configuration with a name, which the oracle keeps. Each codespace uses one preset. |
| Volume | Storage on a host that codespaces mount at a path. It stays when a codespace closes. |
| Workspace | A collection of repositories and mounted volumes, with a name and a host. Most workspaces have one repository and no volume. |
| Codespace | A container on the host of its workspace. It runs one agent with one preset on one workspace. It has a clone of each repository, the mounted volumes, and one or more sessions. |
| Session | One thread with the model. The sessions of a codespace use the same clones. The clients and the code use this word today. |
| Device | A desktop or Android client that is paired with the oracle. |

A preset and a workspace are independent. A preset says how the agent works and which credentials it has. A workspace says what the agent works on, and on which host. A codespace puts one preset and one workspace together.

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

1. The client sends `CreateCodespace` with the workspace, the preset, the resource limits, the base branch of each repository, and the client version. It can also send the first message with its model.
2. The oracle reads the host from the workspace. It makes sure that the host is connected and can take one more codespace. It writes the codespace row and makes a one-time bootstrap secret.
3. The oracle sends the command to `loom-host`. `loom-host` makes the volumes of the workspace that the host does not have. Then it creates the instance from the image of the host, with the volumes.
4. `loom-host` puts in the `loom-agent` binary of the client version, the systemd unit, and the bootstrap file. Then it starts the instance.
5. In the container, systemd starts `loom-agent serve --managed`.
6. The agent makes its own keys. It pairs with the oracle with the bootstrap secret. The secret works one time.
7. The agent gets its bundle: the YAML file, `AGENTS.md`, and the skills of its preset, the secrets, the Git identity, and the device keys.
8. The agent clones each repository, makes the branch `loom/<codespace>` in each clone, and runs the setup scripts.
9. The agent registers with the relay and reports `ready`. The oracle sends the progress of steps 3 to 9 to the client.
10. If there is a first message, the agent starts a session with it. No client is necessary for this step.
11. The client connects to the agent with the selected connection type when you open a session.

### Codespace close

1. The client asks the agent for uncommitted changes and unpushed commits in each clone.
2. If there is such work, you choose for each repository: push the branch of the codespace, or discard the work.
3. The oracle tells `loom-host` to delete the instance. The volumes stay. The oracle revokes the keys and tokens of the codespace.

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
- Each `loom-host` has local rules in its configuration file: the permitted Git providers, the maximum number of codespaces, the permitted images, and the limits for volumes. They also name the host directories that can be volumes, and the host user for them.

### 6.5 Containers get credentials from a broker, not copies (requirements 3, 4, 6)

A copy of an OAuth credential in each container breaks when a provider rotates the refresh token. One container refreshes, and the copies in the other containers become invalid. `loom-llm` already stores a new refresh token when a model provider returns one.

- The oracle keeps the refresh tokens and is the only service that refreshes.
- For models, the agent asks the oracle for a current access token. Sign-in occurs one time at the oracle, not in each container.
- A codespace gets only the credentials that its preset names: model accounts, Git credentials, and secrets. See section 7.7.
- For Git, a credential helper (`loom-agent git-credential`) asks the oracle for the token. The token is not in the image and not in a file.

A classic agent and the oracle cannot share one sign-in for the same reason. Sign in again at the oracle.

### 6.6 Calls to a Git provider can go through a host (requirements 3 and 4)

The oracle cannot reach your GitHub Enterprise Server. The oracle keeps the credential, and it sends each API call for this provider through a `loom-host` that can reach the server. Examples are the search of repositories and the list of branches. `loom-host` sends such calls only to the API URL of a registered provider.

### 6.7 Stop is different from delete

Agents work when no client is connected. Thus, "close" must be an explicit action, not a disconnect. The first version has close with the guard of section 5. Stop on idle and delete after a retention period come in phase 6.

### 6.8 Data that must outlive a container goes to the oracle or to a volume

Today the notes of an agent stay when you delete a session. In a temporary container, they are lost. The oracle keeps the notes for each workspace, and each new codespace gets them. An archive of sessions in the oracle is a later option.

Large data that must stay goes into a volume of the workspace. Examples are caches and data sets. See section 7.14.

### 6.9 The container is the sandbox

In managed mode, the default policy is `allow`, and the landlock or bubblewrap sandbox is off. The container and the limited tokens set the limit of the damage.

### 6.10 The agent version follows the client

The agent protocol needs the same version on both sides. `loom-host` keeps a cache of `loom-agent` releases and installs the version of the client that creates the codespace. The current update command covers later client updates.

During the transition, the releases come from `main`, and their agent has no managed mode. Thus, `loom-host` can also take the agent binary from a local directory. This directory has a build of the branch `loom-next`. See section 8.

## 7. Design details

### 7.1 Store

SQLite with `rusqlite`, which the agent already uses. Numbered migrations.

| Table | Contents |
| --- | --- |
| `peers`, `pairings` | Paired devices, hosts, and codespace agents with their keys and roles. Push tokens of devices. One-time pairing secrets. |
| `relay_hosts` | Host IDs of classic agents. Replaces `allowed_hosts`. Phase 8 removes it. |
| `hosts` | Name, CPUs, memory, storage driver, reachable providers, connection types, environment configuration. |
| `providers` | Name, web URL, API URL, Git identity, route (direct or through hosts). |
| `git_credentials` | Tokens (encrypted) with a name and a scope: provider, owner, or repository. Expiry date. |
| `model_accounts` | The accounts of the model providers: name, email, organization, and the credential of `loom-llm`, encrypted. |
| `secrets` | Names and values (encrypted). |
| `presets`, `preset_files` | The presets of agent configuration: name, YAML file, `AGENTS.md`, and skill files. |
| `workspaces` | Name, host. |
| `workspace_repositories` | The repositories of each workspace in their sequence: provider, owner, repository, directory name, base branch. |
| `volumes` | The volumes: name, kind, optional size limit, and the path of a host directory. The hosts that have each Incus volume. |
| `workspace_volumes` | The volumes of each workspace: volume, mount path, read-only mark. |
| `codespaces` | Workspace, preset, host, CPU limit, memory limit, instance name, state, branch, base branch of each repository, keys of the agent, times. The first message until the agent takes it. |
| `sessions` | The last summary of each session: title, state, time. |
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
- `loom-host` registers its certificate with the trust token through the API. It needs no `incus` client.
- The path of the local socket depends on the package of Incus. `loom-host` finds the socket, or you set its path.
- The server certificate of a host can be a public certificate that changes. `loom-host` uses the system CA and the host name for such a certificate. For a self-signed certificate, it pins the certificate of the token.
- `loom-host` sets the root disk and the network of each instance. It does not depend on the `default` profile of the project.
- `loom-host check` tests the project settings, the storage driver, and Docker in a test instance. It also reports the `zstd` program, the project quotas, and the setup for host directories.
- `loom-host check` also reads the project itself. A restricted certificate without the project gets empty lists, not errors.
- `loom-host` reports the CPUs, the memory, the storage driver, the reachable Git providers, and the connection types.
- The commands from the oracle are: build the image, create, start, stop, delete, and get the status of a codespace instance. For volumes, the commands are make, list, and delete.
- A driver trait has two implementations. `IncusDriver` uses `loom-incus`. `ProcessDriver` starts a local `loom-agent` process, for tests and for development without Incus.
- Each instance has `user.loom.*` keys that name the oracle and the codespace. `loom-host` reports its instances, and the oracle compares them with the `codespaces` table. Orphans are deleted, and lost codespaces get a mark.
- A codespace gets the CPU limit and the memory limit that **New codespace** sets. See section 7.15.
- Phase 7 adds an OCI image of `loom-host` for IncusOS. It has `loom-host`, `loom-agent` of the same build, and the CA certificates of Alpine Linux. Its configuration and data are in `/var/lib/loom-host`, which is a custom volume. It starts `loom-host serve --wait`, which waits until you run `trust` and `join` with `incus exec`. CI builds the image as an OCI archive. A registry for the image is an open decision. A test with `docker run` passed. Incus refused a test registry without TLS, so the launch in Incus is not tested.

### 7.3 Image, Docker, and the environment of a host

- The oracle keeps the environment configuration of each host (requirement 3). It has: the base image, a host setup script, environment variables such as proxy settings, CA certificates, and Docker settings such as registry mirrors.
- `loom-host` builds the codespace image through the Incus API. It starts an instance from the base image, runs the base script of Loom, and then runs the host setup script. Then it publishes the instance as an image.
- `loom-host` publishes the image with zstd compression. If the host has no `zstd` program, it publishes without compression. Phase 0 showed that gzip makes each start slower on `dir`.
- The base script installs git, `gh`, Docker Engine, and a user with `sudo`.
- `loom-host` builds the image again when the environment configuration of the host changes.
- For Docker, each instance has `security.nesting=true`. Phase 0 showed that this is sufficient. Syscall interception is not necessary.
- The restricted project must permit nesting: `restricted.containers.nesting=allow`. `restricted.containers.interception` stays at `block`.

The storage driver of the Incus pool changes the behavior:

| Topic | ZFS (IncusOS hosts) | `dir` on ext4 or XFS (other hosts) |
| --- | --- | --- |
| New instance from the image | A copy-on-write clone. Fast. | A full copy of the image. Slower. |
| Docker storage | Overlay on ZFS needs OpenZFS 2.2 or later. Phase 0 confirmed it with OpenZFS 2.4. The fallback is a block-mode volume for `/var/lib/docker`. | Overlay works on ext4, and on XFS with `ftype=1`. Phase 0 confirmed the two. |
| Start of a new codespace | Fast. Each codespace gets a fresh instance. | 2 to 3 s slower for each 1 GB of image. Each codespace gets a fresh instance, so the image must stay small. |

On IncusOS, you cannot load kernel modules by hand. Phase 0 showed that Docker works there. It also works on test hosts with the `dir` driver on ext4 and on XFS. See section 9.

### 7.4 Bootstrap

- `loom-host` puts three files into a new instance before the start: the `loom-agent` binary, the systemd unit, and the bootstrap file.
- The bootstrap file has the oracle URL, the oracle key, the codespace ID, and the one-time secret. The agent deletes it after use.
- The agent runs as a normal user with `sudo`.
- The file function of the Incus API writes files and symbolic links into a stopped instance. Phase 0 confirmed this.

### 7.5 Git providers (requirement 4)

A provider has a web URL and an API URL. Examples are `https://github.com` with `https://api.github.com`, and `https://ghe.example.com` with `https://ghe.example.com/api/v3`.

The first authentication method is a personal access token, because you do not install a GitHub App in the organization.

| Token type | Reach | Notes |
| --- | --- | --- |
| Fine-grained personal access token | One owner (one organization or your account). All its repositories or the repositories that you select. | Recommended. Your server version 3.18 has it. The organization can require an approval for each token. |
| Classic personal access token | All your repositories. | Works on all instances. |

- A Git credential has a name, a Git provider, and a scope: provider, owner, or repository. Thus, you can use one token for an organization, or a token for only one repository.
- A preset names the Git credentials of its codespaces. For each request, the oracle uses the named credential with the smallest scope that matches.
- A fine-grained token needs these repository permissions: Contents (write), Pull requests (write), and Metadata (read). Add others for the `gh` commands that you use.
- The oracle reads the expiry date of each token from the API responses. The clients show a warning before a token expires.
- A token is long-lived, and each program in the container can ask the helper for it. A fine-grained token with selected repositories limits the damage. The Git proxy of phase 7 removes the token from the container.
- The credential helper is set for the Git provider of each named credential. A codespace gets these credentials and no other credential.
- The agent writes a token for each of these providers into the configuration of `gh`, so `gh` and **Open pull request** work.
- Each provider has its own Git name and email. Each clone uses the name and email of its provider.

A codespace can need more than one Git provider. Examples are a workspace with repositories on two providers, a repository on your GitHub Enterprise Server with dependencies on github.com, and a repository with remotes on two providers.

- For these examples, the preset names a Git credential for each provider. A workspace has no list of providers.
- **New codespace** shows a warning for each repository of the workspace that has no Git credential in the preset. Without a credential, the codespace can only read a public repository.
- Thus, you see it when you select a personal preset for a company workspace.
- The helper sends the path of the repository to the oracle. The oracle selects the credential for each request by provider, owner, and repository. Thus, two owners on one provider can use two tokens.
- `gh` keeps one token for each host. Phase 3 decides which token `gh` gets when a preset names more than one for a provider.
- When the Git credentials of a preset change, the oracle tells the agents of its codespaces. They set the helper and `gh` again.
- The search of repositories and the list of branches use all Git credentials of the oracle for a provider. They are not limited to one preset.

Phase 7 adds the GitHub App method. A Git credential can be an app: the oracle keeps the private key, the ID of the app, and the owner of the installation. The credential helper gets installation tokens for one repository, which expire in 1 hour. The oracle keeps these tokens in memory and makes new ones 5 minutes before they expire. `gh` gets no token from an app, because the token expires. An app reaches only the accounts and organizations where it is installed. This is also true for the user tokens of the app. Thus, personal access tokens stay the method for your GitHub Enterprise Server.

### 7.6 Network reach

- Each `loom-host` tests which Git providers it can reach and reports the result.
- The host of a workspace must reach the Git providers of all repositories of the workspace.
- The oracle makes this check when you save a workspace, and again at each start of a codespace. If the host cannot reach a provider, the oracle refuses and names the provider.
- The host does not have to reach the providers of the other Git credentials of the preset.
- The oracle calls the API of a provider directly when it can. If it cannot, it sends the call through a `loom-host` that can.
- Thus, the Android app outside the company network can also show the repositories of your GitHub Enterprise Server.

### 7.7 Agent configuration and presets (requirement 5)

An agent configuration says how an agent works and which credentials it has. It is for a codespace what `~/.config/loom` and the credential file are for a classic agent.

A preset is an agent configuration with a name. The oracle keeps the presets, and each codespace uses one of them.

| Part | Contents |
| --- | --- |
| YAML file | The settings of `agent.yaml` today, and the names of the credentials. |
| `AGENTS.md` | The instructions for the model. |
| Skills | The skill directories. |
| Credentials | The model accounts, the Git credentials, and the secrets that the YAML file names. The oracle keeps their values. |

- The oracle keeps one or more presets. Each has a name, such as `personal` and `work`.
- The YAML file has only the names of the credentials. Thus, the file itself has no secret values.
- Two presets can name the same credential.
- The oracle validates the YAML file and the names. A name that the oracle does not have is an error.
- The oracle does not merge presets. A codespace uses exactly one.
- There are no global instructions, skills, or secrets. If two presets need the same instructions, each has a copy.
- **New codespace** asks for the preset. A codespace keeps this preset until it closes.
- The managed mode sets some sections itself: `relay`, `socket`, `storage`, `lifecycle`, and `allowed_roots`. It has no worktrees.
- When you change a preset, the oracle tells all codespaces that use it. They write the files, and the current hot reload applies them.

Example of the YAML file of the preset `work`:

```yaml
llm:
  default_model: anthropic/<model>
  accounts:
    anthropic: work
    openai-codex: work
git:
  credentials: [ghe-work, github-acme]
environment:
  secrets:
    NPM_TOKEN: npm-work
    SENTRY_AUTH_TOKEN: sentry
    AWS_SECRET_ACCESS_KEY: aws-work-secret
```

| Key | Meaning |
| --- | --- |
| `llm.accounts` | One model account for each model provider. See section 7.12. |
| `git.credentials` | The Git credentials. See section 7.5. |
| `environment.secrets` | The secrets. Each key is the name of an environment variable in the codespace. Each value is the name of a secret in the oracle. |

The name of a secret in the oracle and the name of its variable in a codespace are independent:

- A secret has one name in the oracle. Each preset gives it the variable name that its tools use.
- Two presets can set the same variable from different secrets. For example, `personal` and `work` each set `NPM_TOKEN` from their own secret.
- One preset can give one secret more than one variable name.
- The agent puts the variables into its env file, which is not in a clone. Thus, shell commands and terminals get them, and the redaction of the agent hides their values from the model.
- When you change a name or a value, the oracle tells the agents. New commands get the new variables.

These keys are new in `loom-config`. Only the managed mode accepts them. Their names can change in phases 3 and 4.

A codespace gets all its other parts from its host and its workspace:

| Source | Parts |
| --- | --- |
| Host | The image, environment variables such as proxy settings, CA certificates, and Docker settings. See section 7.3. |
| Workspace | The repositories with their `AGENTS.md`, skills, and setup script `.loom/setup.sh`. The volumes. |

### 7.8 Connection types (requirement 1)

- Each host has a list of connection types. `relay` is always there. `ssh` is there when you set an SSH destination for the host.
- A client tells the oracle the types that it supports. The desktop client supports the relay and SSH. The Android app supports the relay.
- For each codespace, the oracle gives the types that the two sides support, in the order of preference, with their data.
- The oracle cannot know if a client can reach a host by SSH at this time. Thus, the client tries SSH with a short timeout and then uses the relay.
- The SSH type runs `ssh <destination> loom-host stdio --codespace <id>`. `loom-host` connects the stream to the agent in the container through the exec function of the Incus API.
- Phase 0 confirmed this stream. The example client connected to an agent through the exec function and `loom-agent stdio`. See section 9.
- The SSH destination is the machine where `loom-host` runs. For IncusOS, this is the instance of `loom-host`, which then needs an SSH server and an address that the client can reach.
- As with SSH hosts today, the SSH type trusts the SSH account. The relay type uses the Noise keys.

### 7.9 Codespace states

`provisioning` → `starting` → `ready` → `closing` → `closed`. Phase 6 adds `stopped`. Error states are `failed` and `lost`.

When the oracle restarts, codespaces continue to run. The agents and the hosts connect again, as agents do with the relay today. Only token refresh and new codespaces wait for the oracle.

### 7.10 Lists and events

- Each agent sends a summary of a session to the oracle when it changes: title, state, time, and the archived mark.
- The oracle keeps the last summary. Thus, the lists also show a codespace that is not connected.
- A client subscribes to events on its connection to the oracle: codespace states, start progress, and session summaries.
- The oracle also gives the list of models, because a client needs it before a codespace is there. The list is for the model accounts of one preset.
- The content of sessions does not go to the oracle. The first message is the exception. The oracle keeps it until the agent takes it, and then deletes it.
- Notifications do not change: each agent encrypts them for the devices and sends them through the relay.

### 7.11 Configuration operations

- The control service has all configuration operations: devices, hosts, Git providers, Git credentials, model accounts, secrets, presets, workspaces, and volumes.
- The desktop client is the user interface for them. See section 14.
- The oracle validates each change. It rejects a bad preset and gives the reason, as the agent does today for `agent.yaml`.
- The oracle refuses the delete of a preset or a workspace that a codespace uses. It also refuses the delete of a host that a workspace uses.
- Secrets and tokens are write-only. A client can set, replace, and delete them, but it cannot read them.
- `loom-oracle` has only these commands: `serve`, `config` for the daemon file, and `pair` for the first device.
- A test CLI uses the same control service. It is for tests and for the phases before the screens are there.

### 7.12 Model accounts

You can have more than one account for the same model provider. An example is a personal Claude plan and a Claude plan of your company.

- An account is one sign-in or one API key for a model provider. You give each account a name, such as `personal` and `work`.
- The oracle keeps and refreshes each account separately.
- After a sign-in, the oracle shows the email and the organization that the model provider reports. Thus, you can make sure that the account is the correct plan.
- If you sign in to an account that the oracle already has, the oracle replaces the credential of that account. It does not make a second account.
- A preset names one account for each model provider that it uses. Thus, a codespace uses one account for each model provider.
- The model names, the agent, and the agent protocol do not change.
- A model provider that needs a credential is available only if the preset names an account for it.
- The oracle validates the names. An account that the oracle does not have is an error.
- The broker gives a codespace only the tokens of the accounts in its preset. A program in the container cannot get the tokens of a different account.
- When the accounts of a preset change, the next request to the model uses the new account.
- The agent in a codespace shows the name of its account. Sign-in and sign-out are not possible there.
- The oracle does not change the account automatically, for example when an account reaches its usage limit.

The key `llm.accounts` of the YAML file names the accounts. See the example in section 7.7.

Today, `loom-llm` keeps one Claude account, and a new sign-in replaces it. In the oracle, each account has its own credential store. Thus, the provider code needs no list of accounts.

### 7.13 Workspaces and repositories

A workspace says what an agent works on, and on which host. It is a collection of repositories and mounted volumes, with a name. It has no credentials and no agent settings.

| Setting | Contents |
| --- | --- |
| Repositories | One or more repositories in a sequence. Each has a Git provider, a directory name, and a base branch. |
| Volumes | The mounted volumes. Each has a path and a read-only mark. See section 7.14. |
| Host | The host of the codespaces of the workspace. |

- All codespaces of a workspace run on its host. **New codespace** does not ask for a host, and the oracle does not select one.
- The oracle validates a workspace when you save it. The host must reach the Git providers of the repositories.
- If the host is not connected or has its maximum number of codespaces, the oracle refuses a new codespace. It gives the reason.
- You can change the host of a workspace. The codespaces that run stay on the old host, and the volumes do not move.
- For the same repositories on a second host, make a second workspace.
- A workspace has no resource limits. Each codespace gets its limits from **New codespace**. See section 7.15.

Some work changes more than one repository at the same time. Thus, a workspace has one or more repositories. The repositories can be on different Git providers.

- The repositories have a sequence. The setup scripts run in this sequence.
- One repository can be in more than one workspace.
- You make and change workspaces in the desktop client. New codespaces get the changes.
- **New workspace…** has a short way for one repository. You select a repository in the search and a host.
- A codespace has a clone of each repository.

| Workspace | Directory of each clone | Root directory of the agent |
| --- | --- | --- |
| One repository | `/workspaces/<repository>` | The clone. |
| More than one repository | `/workspaces/<workspace>/<repository>` | `/workspaces/<workspace>` |

- The directory name is the repository name. You can set a different name in the workspace, for example when two repositories have the same name.
- Each clone gets the branch `loom/<codespace>`. The workspace can set a base branch for each repository. The default is the default branch of the repository. **New codespace** can select a different base branch.
- Each clone uses the Git name and email of its provider.
- The agent reads `AGENTS.md` and the skills of each repository.
- The **Changes** panel shows the changes of each repository.
- The close guard examines each clone. For each repository with work, you push the branch or discard the work.
- **Open pull request** opens one pull request for each repository that has commits. Each pull request has links to the others.

Today, the agent uses the one repository at its root directory for the instructions, the **Changes** panel, and the Git actions. In managed mode, the agent gets the list of repositories from the oracle. The agent protocol needs Git commands that name a repository. Phase 3 decides if this needs a version bump.

### 7.14 Volumes

A volume is storage on a host that codespaces mount at a path. Examples of its contents are the caches of package managers and builds, and data sets. There are two kinds:

| Kind | Contents |
| --- | --- |
| Incus volume | A custom storage volume of Incus. `loom-host` makes it in the storage pool of the host. |
| Host directory | A directory that is on the host already, for example on an NFS mount. The volume has the path of the directory. |

These rules apply to the two kinds:

- A volume has a name in the oracle.
- A workspace gives each of its volumes a mount path and a read-only mark. Two workspaces can use the same volume.
- A volume stays when a codespace closes. The close guard does not examine volumes.
- All codespaces that use a volume on a host mount it at the same time. Thus, the programs that write to it must permit this.
- The oracle validates the mount paths. A path cannot be a system directory or `/workspaces`. Phase 3 decides if a path in a clone is possible.

Incus volumes:

- The data of a volume is on one host. `loom-host` makes the volume when the first codespace on that host needs it. A new volume is empty.
- The oracle does not copy a volume between hosts. Thus, two workspaces share the data of a volume only when they have the same host.
- If you change the host of a workspace, its new codespaces get new, empty volumes on the new host.
- The user of the agent owns a new volume.
- A volume has no size limit, unless you set one.
- A size limit applies on ZFS. With the `dir` driver, it needs project quotas on the file system. Without them, Incus accepts the limit but does not apply it.
- The oracle deletes a volume only when you ask for it. Incus refuses the delete while a codespace uses the volume.

Host directories:

- This kind is for Linux hosts with a shell. An example is a directory of a company NFS server that the host mounts.
- `loom-host` permits only the paths that its local rules name. The oracle cannot change these rules.
- The Incus project has the same paths in `restricted.devices.disk.paths`. It refuses a disk from a different host path.
- The oracle asks `loom-host` when you save the workspace. The host of the workspace must permit the path, and the directory must be there.
- More than one host can mount the same NFS directory at the same path. Then the volume has the same data on these hosts.
- Loom does not make or delete a host directory, and a size limit does not apply.
- Without an ID map, the files belong to `nobody` in the container. The agent can read the files that all users can read, and it cannot write.
- For write access, the local rules name one user and one group of the host. The user of the agent in the container then has these IDs on the host. Incus does this with `raw.idmap`.
- If a host has this ID map, all codespaces on that host use it. Thus, an Incus volume has the same owner in all codespaces of the host.
- A host has only one such user. Thus, a codespace writes only to the directories that this user can write to.
- Only the user of the agent gets the IDs of the host user. Root in the container and root in a Docker container cannot write to the directory. `docker run --user` with the IDs of the agent user can.
- The ID map needs a setup on the host that only its administrator can do: entries in `/etc/subuid` and `/etc/subgid`. `loom-host check` reports what is missing.
- Other methods do not work for NFS. An NFS mount has no ID-mapped mounts, and an Incus storage pool on an NFS mount with root squash failed.

Example of the workspace `acme` with two repositories, two volumes, and a host:

| Part | Value | In the codespace |
| --- | --- | --- |
| Repository | `ghe.example.com/acme/server` | `/workspaces/acme/server` |
| Repository | `github.com/acme/client` | `/workspaces/acme/client` |
| Volume | `acme-data`: the host directory `/mnt/nfs/data/acme`, read-only | `/workspaces/acme/data` |
| Volume | `build-cache`: an Incus volume | `/cache/build` |
| Host | `office-1` | The container runs on this host. |

Phase 0 tested Incus volumes on all test hosts, and NFS directories on the test host with Enterprise Linux 9. See section 9.

### 7.15 Resource limits

A host runs many temporary containers at the same time. Thus, each codespace has a CPU limit and a memory limit.

- **New codespace** sets the two limits: the number of CPUs and the memory.
- There are no size profiles. A workspace and a host do not set these limits.
- The dialog starts with the limits of the last codespace of the workspace. For the first codespace of a workspace, it starts with 2 CPUs and 4 GiB of memory.
- `loom-host` sets `limits.cpu` and `limits.memory` on the instance when it creates the instance.
- The oracle refuses a limit that is more than the CPUs or the memory of the host.
- If the programs in a codespace use more memory than the limit, the kernel stops a process in that codespace. Other codespaces continue.
- A limit is a maximum, not a reservation. Thus, the sum of the limits on a host can be more than the host has.
- A codespace keeps its limits until it closes.
- A codespace has no disk limit. Its disk space comes from the storage pool of the host.
- A volume can have its own size limit. See section 7.14.

Phase 0 tested the two limits, and a root disk without a size, on the IncusOS host. See section 9.

### 7.16 Network rules (phase 7)

Phase 0 showed that a container reaches the Incus API port of its host and the other instances on the bridge. The network rules are local rules of `loom-host` in `host.yaml`. The oracle cannot change them.

- `rules.network.isolate` is on by default. `loom-host` then sets `security.port_isolation`, `security.mac_filtering`, `security.ipv4_filtering`, and `security.ipv6_filtering` on the network interface of each container.
- Port isolation stops the traffic between codespaces. The instance of `loom-host` on IncusOS stays reachable, because it does not have the option.
- The filters stop a container that uses the MAC or IP address of a different instance. Thus, the Git proxy can identify a codespace by its address. See section 7.18.
- Isolation does not stop the traffic to the host and to other networks. For this, `rules.network.acls` names Incus network ACLs. `loom-host` adds them to the network interface with the default actions of `host.yaml`. The default action is `allow`, so an ACL lists only the traffic to stop.
- A restricted certificate cannot make ACLs, because a shared bridge has its ACLs in the `default` project. Thus, the administrator makes them with the `incus` CLI. `loom-host check` reports an ACL that does not exist.

Test on the first host (2026-11-24), with the restricted certificate and Incus 7.6:

- The project accepted the four options.
- Two containers with port isolation could not reach each other. Each reached a container without the option, the host, and the Incus API port.
- A container with the filters could not send from a second IPv4 address. A container without the filters could.
- Docker in a container with the options pulled an image and reached the internet.
- The request to make an ACL failed: "User does not have permission for project default". The ACLs themselves are not tested.

### 7.17 Repository setup and `devcontainer.json` (phase 7)

The agent runs the setup of each repository after the clones, in the sequence of the workspace. It runs as the agent user in the clone. The host setup script runs before, as root, in the image.

- `.loom/setup.sh` comes first. If a repository has it, the agent does not read `devcontainer.json`.
- Else the agent reads `.devcontainer/devcontainer.json` or `.devcontainer.json`, with comments and trailing commas.
- The subset: `containerEnv` and `remoteEnv`, `onCreateCommand`, `updateContentCommand`, `postCreateCommand`, and `postStartCommand`, with the common variables such as `${containerWorkspaceFolder}`.
- The image, Dockerfiles, features, mounts, and ports of `devcontainer.json` are not in the subset. The host sets the environment, as before. Full support stays out of the plan (section 12).
- The variables of `devcontainer.json` go to the setup commands, the shell commands, and the terminals. They are not secrets, so the redaction does not hide them. The variables of the preset have priority.
- The one-time commands run one time for each clone. `postStartCommand` runs at each start of the agent.
- A failed command does not make the codespace fail. GitHub Codespaces also continues in this case. The progress shows the failure, and a log file in the codespace has the output.

### 7.18 Git proxy (phase 7)

The Git proxy keeps the tokens of the providers out of the codespaces. It is optional: `git_proxy` in `host.yaml` turns it on for a host.

- `loom-host` listens for Git over HTTP on an address on the bridge. The host report has the URL of the proxy.
- For a codespace on such a host, the Git setup has a proxy URL for each provider: `<proxy>/<provider>`. The agent writes `url.<proxy>/<provider>/.insteadOf <web URL>/`, and the credential helper for the proxy URL. `gh` gets no token.
- The credential helper gets the codespace ID and a random proxy secret from the oracle. The oracle keeps the secrets in memory. After a restart of the oracle, the codespaces get new secrets. The oracle gives no provider token to a codespace on a host with a proxy.
- The proxy accepts only `info/refs`, `git-upload-pack`, and `git-receive-pack` for `<provider>/<owner>/<repository>`. It checks that the address of the client is an address of the instance of the codespace. Then it asks the oracle with the new host command `git_proxy_access`. The oracle checks the secret, the host, and the state of the codespace. It gives the URL of the repository on the provider and the token of the preset, or no token. The proxy adds the token and streams the request and the answer. Thus, the proxy is not an open proxy: the oracle makes the upstream URL from the provider.
- The agent opens pull requests through the new codespace command `codespace_api_request`. The oracle permits only `GET` and `POST` on `/repos/<owner>/<repository>/pulls` and `PATCH` on `/repos/<owner>/<repository>/pulls/<number>`, for the repository of the URL, and adds the token.
- The proxy needs the address filters of section 7.16. The traffic on the bridge is HTTP.
- Not in phase 7: Git LFS, Git over SSH, and the process driver.

## 8. Compatibility with the current Loom (requirement 7)

- The development branch of Loom-Next is `loom-next`. It starts from `main`.
- No Loom-Next branch merges into `main` until the transition is finished. The transition is finished when phase 8 is complete.
- Each phase has its own branch with the name `loom-next-phase-<n>`, for example `loom-next-phase-1`. The branch starts from `loom-next`.
- When a phase is complete, a pull request brings its branch into `loom-next`. Then the branch of the next phase starts from `loom-next`.
- At the end of the transition, one pull request brings `loom-next` into `main`.
- Thus, `main` has only the current Loom during the transition. The work on the current Loom continues on `main`, with its releases.
- We rebase `loom-next` onto `main` at regular intervals, so `loom-next` has all changes of `main`. An open phase branch then rebases onto `loom-next`.
- The new code is in new crates and behind new entry points. Thus, the rebases stay small, and a phase does not change the classic modes.
- The agent protocol does not change for Loom-Next, if possible. New commands go at the end of `Command`, which needs no version bump.
- On `loom-next`, `loom-relay`, `relay.yaml`, local hosts, SSH hosts, relay hosts, and worktree sessions continue to work until phase 8.
- `client.yaml` gets a new host kind. Old kinds do not change until phase 8.
- The normal checks (fmt, clippy, tests) run for the old and the new code in each commit.
- CI runs for each pull request, also for a pull request into `loom-next`. It builds the archives of the binaries and the Android app.
- The releases contain no Loom-Next code during the transition. The first release with Loom-Next comes after the merge of `loom-next` into `main`.
- Until then, all binaries of Loom-Next come from builds of `loom-next`: `loom-oracle`, `loom-host`, the agent with the managed mode, and the clients.
- There are no pre-releases from `loom-next`. You ask for one when you need it.
- From phase 3, the build workflow also makes archives of `loom-oracle` and `loom-host`.

## 9. Phases

Each phase ends with a state that you can use and test. Each phase has its own branch. See section 8. Sizes are relative: S, M, L.

| Phase | Size | Work | Result |
| --- | --- | --- | --- |
| 0. Spike | S | By hand with the `incus` CLI on one IncusOS host and one Linux host: restricted project, container, Docker, current `loom-agent`, current relay. Measure the start time. Test the connections from the container to the relay, the GitHub instances, and the model providers. | Proof that an unchanged agent and Docker work in a container on the two host types. Real numbers for the start time. |
| 1. Oracle as relay | M | Host check trait and limits for each peer in `loom-relay`. `loom-oracle serve`, store, secret encryption. Pairing with roles. Control service with `Hello`, events, and the device operations. Client library and the test CLI. | The current agents and clients work with the oracle as their relay. A paired device can call the control service. |
| 2. Hosts and codespaces | L | `loom-host` with `join`, `check`, the link to the oracle, and `ProcessDriver`. `IncusDriver` on `loom-incus` with the socket and with a certificate. Image build with the environment of the host. Agent cache. Codespace state machine and instance comparison. Workspaces with a host and one public repository. CPU and memory limits of a codespace. Managed mode: bootstrap, keys, clone of a public repository, `ready`. | The test CLI makes a codespace for a workspace on an IncusOS host and on a Linux host. The current desktop client connects to it with a relay host entry made by hand. |
| 3. Git providers and workspaces | L | Providers and Git credentials with scopes. Presets that name Git credentials. Workspaces with more than one repository and with volumes. Volumes of the two kinds: Incus volumes, and host directories with the ID map. Calls through a host. Check of the network reach of the host of a workspace. Repository search and branch list. Credential helper, Git identity, `gh`. Git actions for each repository of a codespace: status, push, pull request. Close guard. The first test with your GitHub Enterprise Server. | Codespaces for private repositories on github.com and on your GitHub Enterprise Server can clone, push, and open a pull request. One codespace has clones of two repositories on the two providers, a volume that stays after the close, and a directory of your NFS server. |
| 4. Presets and model credentials | M | The full preset: YAML file, `AGENTS.md`, skills, secrets with their variable names, and live update. Instructions and skills of each repository. Credential source trait in `loom-llm`, broker, sign-in at the oracle. Model accounts. Notes for each workspace. Managed defaults for policy and sandbox. First message. Session summaries. | A new codespace needs no setup and no sign-in, and it starts its first turn without a client. Ten codespaces stay signed in for longer than one token lifetime. Two codespaces with different presets use different accounts of one model provider. |
| 5a. Clients: daily work | L | Desktop and Android: host kind `oracle`, pairing, **Workspaces**, **New codespace** with a workspace, a preset, and the first message, progress view, lists from the oracle, **Changes** for more than one repository, codespace menu, notifications. A first version can start after phase 2. | You do all daily work from the desktop client and the Android app. |
| 5b. Desktop client: configuration | L | The configuration pages of section 14. | You do all configuration in the desktop client. |
| 6. Lifecycle and SSH | M | Stop and resume, idle timeout, retention, limits for each host, metrics, audit. The SSH connection type and its selection. | Codespaces clean up themselves. The desktop client uses SSH where it can. |
| 7. Hardening | L | Network rules for containers. A Git proxy that keeps tokens out of containers. GitHub App method. OCI image of `loom-host`. VM instances. `devcontainer.json` subset. Session archive. | Codespaces cannot reach each other, and provider tokens stay out of containers. |
| 8. Removal of the classic modes | M | Remove local hosts, SSH hosts, relay hosts for directories, worktree sessions, and the pairing with agents. Remove the code that only they use. Clean up the agent protocol with one version bump. | Loom has one mode. |

### Phase 0: results on the first host

The first test host is an IncusOS host with Incus 7.6, kernel 7.2, and OpenZFS 2.4. The test dates are 2026-11-19, 2026-11-20, and 2026-11-22.

The machine where we develop Loom is a container on this host, outside the test project. It had the role of the `loom-host` instance in section 7.2. All operations used the REST API with a restricted certificate for one project. The test needed no `incus` client and no shell on the host.

These commands make such a project:

```
incus project create <project>
incus profile device add default root disk path=/ pool=<pool> --project <project>
incus profile device add default eth0 nic network=<bridge> name=eth0 --project <project>
incus project set <project> restricted=true restricted.containers.nesting=allow restricted.networks.access=<bridge>
incus config trust add <name> --projects <project> --restricted
```

The test project also permitted syscall interception, which is not necessary. After the registration, the trust entry had an empty project list. `incus config trust edit` added the project. The cause is not known.

| Step | Time |
| --- | --- |
| First container from a remote image, with the download | 15 s |
| Create a container from a cached image | 0.2 s |
| Start | 0.2 s |
| Network address after the start | 0.6 s |
| Stop | 0.8 s |
| Delete | 0.1 s |
| Install Docker Engine in the container | 24 s |
| Publish the container as an image (275 MiB) | 4 s |
| First container from the new image | 2.5 s |
| From the create request to a connected client | 3 s |

The last row has these steps: create the container, put in the agent binary, the unit, and the configuration, start, clone a small repository, register with the relay, and connect a client. The first run after a new image took 5.7 s.

Results:

- **API.** Registration with a trust token, instances, files, exec, image publish, and profiles work with a restricted certificate.
- **Docker.** Docker Engine 29 works with `security.nesting=true` only. Without nesting, `docker run` fails.
- **Docker tests.** Pull, build, run, networks, published ports, volumes, Compose, memory limits, privileged containers, and Docker in Docker.
- **Device nodes.** `mknod` for a device node fails on the file system of a Docker container, with and without syscall interception. Normal images do not need it.
- **Agent.** The unchanged `loom-agent` 0.5.0 runs as a systemd service of a normal user. It registers with the relay 50 ms after its start and uses 17 MB of memory.
- **Client.** A client paired with the agent and connected to it through the relay.
- **Exec with WebSockets.** The exec function gives one WebSocket each for stdin, stdout, stderr, and control. Binary data passes without change, and the exit code comes back. A terminal with a window size also works. The example client connected to the agent through this stream and `loom-agent stdio` in 0.1 s.
- **Limits.** With `limits.cpu=2`, the container has 2 CPUs. With `limits.memory=512MiB`, it has 512 MiB of memory, and the kernel stops a process that uses more. A root disk without a size has the free space of the storage pool.
- **Restrictions.** The project refuses privileged containers, `raw.lxc`, disks from host paths, and physical network devices.
- **Storage pools.** The project can use all storage pools of the host. The documentation has `restricted.storage-pools.access` to limit them. This is not tested.
- **Volumes.** A restricted certificate makes a custom storage volume in 0.1 s. Two containers mounted one volume at the same time, and the normal user of each container wrote to it. A Docker container used the volume as a bind mount. The size limit and the read-only mark apply. A volume without a size limit has the free space of the storage pool. The data stayed after the delete of all containers. Incus refused the delete of a volume in use.
- **Network.** The container reaches GitHub, the model providers, Docker Hub, and the relay. It also reaches the Incus API port of the host and the other instances on the bridge.

### Phase 0: results on a host with the `dir` driver

This test host is a virtual machine in the test project of the first host. It is not one of your Linux hosts. It has Debian 13, kernel 6.12, an ext4 file system without project quotas, and Incus 6.0.4 from Debian. The storage driver is `dir`. The test date is 2026-11-22.

The setup used the way of `loom-host` on a Linux host. The REST API on the local socket made the restricted project and its profile. A new certificate registered with a trust token. This time, the trust entry had the project. The tests then used the HTTPS API with the restricted certificate.

| Step | `dir` on ext4 | ZFS (first host) |
| --- | --- | --- |
| First container from a remote image, with the download | 15 s | 15 s |
| Create a container from the codespace image (1 GB on disk) | 1.9 to 2.7 s | 0.2 s |
| Start | 0.2 s | 0.2 s |
| Stop | 0.8 s | 0.8 s |
| Delete | 0.4 s | 0.1 s |
| Publish the container as an image (295 MiB) | 3.4 s | 4 s |
| From the create request to a connected client | 5.7 to 6.4 s | 3 s |

Results:

- **Docker.** Docker Engine 29 works with `security.nesting=true` only. All Docker tests of the first host gave the same results.
- **Agent and client.** The unchanged agent registered with the relay, and the client connected. Exec with WebSockets gave the same results.
- **Limits.** The CPU limit and the memory limit apply.
- **Disk.** Incus accepts a size for the root disk and for a volume, but it does not apply the size. The container sees the full file system of the host.
- **Volumes.** All other volume tests gave the same results: two containers at the same time, a Docker bind mount, the read-only mark, and the delete guard.
- **Restrictions.** The project of Incus 6.0.4 refuses the same requests.

### Phase 0: results on a host with Enterprise Linux 9

Your Linux host has an Enterprise Linux 9 distribution, kernel 5.14, an XFS root file system, and an NFS mount. This test host is a stand-in for it: a virtual machine in the test project of the first host. The test date is 2026-11-22.

The virtual machine has Rocky Linux 9 with the kernel version of your host, 5.14.0-687.64.1.el9_8. The Incus pool is on XFS with the driver `dir`. Incus 7.0.0 comes from the COPR repository `neelc/incus`. Your host has Incus 7.0.2. SELinux and firewalld were off. They are also off on your host.

| Step | Time |
| --- | --- |
| Create a container from the codespace image, zstd compression | 1.8 to 2.7 s |
| Create a container from the codespace image, gzip compression | 5.5 to 5.8 s |
| Start | 0.2 s |
| Publish the container as an image: zstd, gzip | 3.3 s, 28 s |
| From the create request to a connected client: zstd, gzip | 4.8 to 6.3 s, 8.6 to 9.1 s |

Results:

- **Same results.** These work as on the other hosts: Docker with nesting only, the agent, the client, exec with WebSockets, the limits, the restrictions, and Incus volumes.
- **Compression.** The host had no `zstd` program. Incus then refused zstd, and its default was gzip. An image with gzip made each new container 3 s slower.
- **Project quotas.** With the XFS mount option `prjquota`, the size of a volume and of a root disk applies. Without it, Incus does not apply the sizes.
- **Socket.** The local socket is `/run/incus/unix.socket`. On Debian, it is `/var/lib/incus/unix.socket`.
- **ID ranges.** `/etc/subuid` and `/etc/subgid` had no entry for root. The Incus documentation gives the entry that containers need.

Results for NFS directories as volumes. The test used an NFS 4.2 export with root squash, and a directory that a normal host user owns:

- **Paths.** With `restricted.devices.disk=allow` and `restricted.devices.disk.paths`, the project accepts a disk from a host path below the listed paths. It refuses other paths.
- **Without an ID map.** The directory belongs to `nobody` in the container. The agent user and root can read it and cannot write to it.
- **`shift=true`.** Incus refuses this property for a disk from such a path.
- **`raw.idmap`.** This property maps the host user to the agent user. Then the agent user reads and writes, and the NFS server sees the host user. The project permits the map with `restricted.idmap.uid` and `restricted.idmap.gid`. The host needs an entry for the host user in `/etc/subuid` and `/etc/subgid`.
- **Root.** Root in the container and root in a Docker container cannot write to the directory. `docker run --user` with the IDs of the agent user can.
- **Other tests.** Two containers at the same time, the read-only mark, and `git clone` into the directory work. The start time does not change with the ID map.
- **Storage pool.** An Incus storage pool on the NFS mount failed, because root has no rights there.

### Phase 0: consequences and open tests

Consequences for the design:

- On ZFS, the start of a container is not the slow part. The clone and the setup script are. Thus, workspace templates would help only for large repositories and long setup scripts. They are not in this plan.
- On `dir`, each new container is a full copy of the image. The copy took 2 to 3 s for 1 GB, and it grows with the image. Thus, hosts with `dir` need a small image. Prepared instances would save this time, but they are not in this plan.
- On `dir`, Incus unpacks the image for each new container. Thus, `loom-host` uses zstd compression or no compression, not gzip.
- Incus 6.0 is sufficient. The tests used only API functions that Incus 6.0.4 has.
- Enterprise Linux 9 with kernel 5.14 and XFS is sufficient for a host.
- A volume from an NFS directory is a disk from a host path with `raw.idmap`. See section 7.14.
- `loom-host` must find the local socket, because its path depends on the package.
- The SSH connection type of section 7.8 is possible. `loom-host` can connect a client to `loom-agent stdio` through the exec function.
- `loom-incus` sends an empty text frame for the end of stdin. For a terminal, it must close the socket after the end frame. If it does not, the operation does not end.
- A command stops when the client closes its sockets. Thus, a lost connection leaves no process in the container.
- `loom-host check` must report if size limits of volumes apply on the host.
- The exec function with a user ID does not set the other groups of the user. A process that needs the `docker` group must start through systemd or `su`.
- The managed mode must set `allowed_roots` and the runtime directory, as section 7.7 says. The classic defaults do not fit a container.

Not tested yet:

- One of your Linux hosts. The tests used virtual machines.
- SELinux in enforcing mode, and firewalld in operation. They are off on your host, so your host does not need this test. A host that has them in operation needs it.
- Your NFS server: its export options, and group permissions.
- Volumes for containers with isolated ID maps.
- The connection to a GitHub Enterprise Server. Phase 3 tests it.
- A turn with a model. The container had no model credential. Phase 4 tests it.

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
| Long-lived tokens for the Git providers are in the oracle. A program in a container can read all Git credentials and secrets of its preset. | A preset names only the credentials that its work needs. Fine-grained tokens with selected repositories and an expiry date. The Git proxy of phase 7. |
| All containers of the company network have one public IP address. The relay limits connections and messages for each IP address. | Limits for each registered peer in phase 1. |
| Docker does not work in a container on one of your Linux hosts, because the host is different from the test hosts. An example is SELinux in enforcing mode. | Phase 0 showed that it works on IncusOS with ZFS, and on test hosts with `dir` on ext4 and on XFS. One test host had the kernel version of your host. `loom-host check` tests Docker on each host. The fallback is VM instances. |
| Hosts with the `dir` driver start codespaces slowly. | Phase 0 measured 2 to 3 s more for an image of 1 GB. A small image with zstd compression. |
| Version skew: old containers, a new client, and two clients of different versions. | The agent version follows the client. The current update command. JSON in the control service. |
| Lost work when a container is deleted. | Close guard, push of the branch. Stop and retention warnings in phase 6. |
| A container can reach the network of its host. Phase 0 confirmed this for the Incus API port and for other instances on the bridge. | Network rules in phase 7. Before that, the host setup document recommends a separate bridge for codespaces. |
| Docker in the container needs nesting, which makes the isolation weaker. | Unprivileged containers in a restricted project. VM instances as an option in phase 7. |
| Disk use: each codespace has full clones, its build output, and Docker images. A codespace has no disk limit, so one codespace can fill the storage pool of its host. Volumes use more disk space. | Codespaces are temporary. Optional size limits of volumes, retention, registry mirrors in the Docker settings of the host. A disk limit is a later option. |
| The codespaces on a host use more memory than the host has, because a limit is not a reservation. | The memory limit of each codespace. The maximum number of codespaces of each host. Idle timeout in phase 6. |
| The configuration pages are a large part of the desktop work. | Text editors with validation for files. Forms only for lists. The test CLI until the pages are there. |
| After phase 8, all work needs an oracle and an Incus host. | `ProcessDriver` stays for the development of Loom. |
| A codespace uses the wrong preset, for example the personal Claude plan for the code of your company. | **New codespace** starts with the preset of the last codespace of the workspace. It shows the model accounts of the preset, and a warning when a repository has no Git credential. The lists show the preset of each codespace. |
| A workspace has one host. When this host is down or full, the workspace gets no new codespace. | You can change the host of the workspace, or make a second workspace on a different host. The volumes do not move. |
| `loom-next` and `main` move apart, because the transition is long and `main` continues to change. | New crates and new entry points. Small changes to the shared crates. Rebases of `loom-next` onto `main` at regular intervals. |
| Loom-Next is in no release before the one merge at the end. Thus, problems can show late, and the merge is large. | Each phase ends with a state that you can use. You use builds of `loom-next` for daily work from phase 5a. The checks run for each commit. |
| The agent, the **Changes** panel, and the Git actions work with one repository today. | In managed mode, the agent gets the list of repositories from the oracle. Phases 1 and 2 use one repository. Phase 3 adds the list. |
| A volume carries files from one codespace to the next. A bad program can change files that a later codespace uses, possibly with different credentials. | The read-only mark. Separate volumes for personal and company workspaces. The clients show the volumes of each workspace. |
| A volume is on one host and has no copy. Two codespaces can write to it at the same time. | Use volumes for caches and for data that you can get again. No automatic delete. The backup of a host is not a task of Loom. |
| A codespace with a host directory has the rights of a host user in that directory. A bad program can change or delete shared files on your NFS server. | The read-only mark. A short list of paths in the local rules of `loom-host`. A separate host user with limited rights for Loom. |
| The ID map gives the agent user the IDs of a host user. A wrong setup can give a codespace more rights than you want. | The oracle cannot set the IDs or the paths. They are in the local rules of `loom-host` and in the Incus project. |

## 12. Not in this plan

- A web client.
- Configuration functions in the Android app.
- Full support of `devcontainer.json` with Docker features.
- Public URLs for forwarded ports.
- More than one user with separate permissions.
- A move of old sessions from classic agents into Loom-Next.
- An automatic change of the model account, for example at a usage limit.
- Instructions, skills, or secrets that all presets share.
- An automatic selection of the host for a codespace.
- A change of the preset or of the resource limits of a codespace that runs.
- A disk limit for a codespace.
- A copy or a synchronization of volumes between hosts.
- Workspace templates: a saved instance with the clones and the result of the setup, from which a new codespace starts. A later plan can add them if large repositories or long setups make the start slow.
- Prepared instances: containers that a host makes before a request, so that a new codespace on a host with `dir` does not wait for the copy of the image. Each codespace starts from a fresh instance.

## 13. Open questions

There are no open questions at this time.

## 14. Clients

### Structure

Today the structure is host → agent → sessions. The new structure is oracle → workspace → codespace → sessions. A codespace takes the place of the agent. Thus, the session view, terminals, files, changes, ports, notes, and tasks do not change.

- `client.yaml` gets the host kind `oracle`. You pair with the current `loom://` dialog or QR code.
- The groups by state stay: Needs approval, Working, Completed, Idle, Archived. Each row is a session. It shows the workspace and the branch of its codespace.
- **Workspaces** takes the place of **Remotes**. It shows each workspace with its host and its codespaces. For each codespace, it shows the branch, the preset, the number of sessions, and the status.
- The lists come from the oracle. See section 7.10. A client connects to an agent only when you open a session.
- The model list of a codespace shows the account name next to each model provider.
- The **Changes** panel groups the changes by repository, when the workspace has more than one.
- While the classic modes are there, the panel shows **Remotes** and **Workspaces**. Phase 8 removes **Remotes**.

### New codespace

1. `ctrl-n` or **+** opens the list of your workspaces.
2. A dialog asks for the preset, the CPU limit, the memory limit, and the base branch of each repository.
3. The dialog starts with the preset and the limits of the last codespace of the workspace.
4. The dialog shows the host of the workspace and the model accounts of the preset. It shows a warning if a repository has no Git credential in the preset.
5. You can type the first message and select its model immediately. The oracle keeps the message and gives it to the agent when the codespace is ready.
6. A progress view shows the steps of the start, and the logs of the setup scripts.

Thus, you can start a task on the phone, close the app, and get a notification when the turn is complete.

### Codespace menu

- **New session** adds a session in the same codespace.
- **Open pull request** pushes the branch of the codespace and opens a pull request, for each repository that has commits.
- **Close codespace** shows the guard and then removes the container.
- **Merge worktree** has no successor. A codespace ends with a push or a pull request.

### Configuration in the desktop client

Each oracle has a **Settings** tab with these pages:

| Page | Contents |
| --- | --- |
| Devices | The paired devices. Add a device with a link and a QR code. Revoke a device. |
| Hosts | The hosts with their status and the results of `loom-host check`. Add a host with a join link. Environment configuration and SSH destination of each host. The volumes on each host, with their sizes. |
| Git providers | URLs, Git name and email. The Git credentials of each provider: name, token, scope, and expiry date. |
| Models | The accounts of each model provider: sign-in or API key, name, email, and organization. |
| Secrets | The names of the secrets. You can set, replace, and delete a value. The presets that use each secret, with the variable names. |
| Presets | The YAML file, `AGENTS.md`, and the skills of each preset. The credentials that each YAML file names. |
| Workspaces | The repositories, the volumes, and the host of each workspace. For each volume, its kind, and the optional size limit or the path on the host. |

- Files such as the YAML files, `AGENTS.md`, the skills, and the host setup scripts open in the text editor of the file view. The oracle validates a file when you save it.
- Lists such as devices, hosts, tokens, and secrets use forms.
- **Import from this machine** reads `~/.config/loom`. Your `agent.yaml`, `AGENTS.md`, and skills become the first preset.
- **Duplicate** makes a copy of a preset with a new name.
- **New workspace…** has a short way for one repository. You select a repository in the search and a host.
- The first device is the exception. `loom-oracle pair` on the oracle machine shows its link. After that, the desktop client pairs the other devices.
- The local rules of a `loom-host` are not in the client. They stay in the file on the host, so that the oracle cannot change them.

### Android

- The same structure as the desktop client for daily work.
- No configuration pages. The app shows warnings, such as a token that expires soon.
- The app has the same **New codespace** dialog. It cannot make or change workspaces, presets, or accounts. Thus, the work on a new repository starts in the desktop client.
- The app registers its push token with the oracle. Each agent gets the token and the key of the device in its bundle.
- The app gets the keys of each codespace from the oracle, so it can open their notifications.

## Sources

- GitHub Codespaces: [deep dive](https://docs.github.com/en/codespaces/about-codespaces/deep-dive), [lifecycle](https://docs.github.com/en/codespaces/about-codespaces/understanding-the-codespace-lifecycle), [repository access](https://docs.github.com/en/codespaces/managing-your-codespaces/managing-repository-access-for-your-codespaces), [secrets](https://docs.github.com/en/codespaces/managing-your-codespaces/managing-development-environment-secrets), [prebuilds](https://docs.github.com/en/codespaces/prebuilding-your-codespaces), [export to a branch](https://docs.github.com/en/codespaces/troubleshooting/exporting-changes-to-a-branch).
- GitHub tokens: [personal access tokens](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens), [endpoints for fine-grained tokens on 3.18](https://docs.github.com/en/enterprise-server@3.18/rest/authentication/endpoints-available-for-fine-grained-personal-access-tokens), [token policy of an organization](https://docs.github.com/en/enterprise-server@3.20/organizations/managing-programmatic-access-to-your-organization/setting-a-personal-access-token-policy-for-your-organization), [expiry header](https://github.blog/changelog/2021-07-26-expiration-options-for-personal-access-tokens/).
- GitHub Apps: [installation tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app), [user tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-with-a-github-app-on-behalf-of-a-user), [installation requests](https://docs.github.com/en/apps/using-github-apps/requesting-a-github-app-from-your-organization-owner).
- Incus: [REST API](https://linuxcontainers.org/incus/docs/main/rest-api-spec/), [authentication](https://linuxcontainers.org/incus/docs/main/authentication/), [projects](https://linuxcontainers.org/incus/docs/main/reference/projects/), [syscall interception](https://linuxcontainers.org/incus/docs/main/syscall-interception/), [Docker in a container](https://linuxcontainers.org/incus/docs/main/faq/), [storage drivers](https://linuxcontainers.org/incus/docs/main/reference/storage_drivers/), [ZFS driver](https://linuxcontainers.org/incus/docs/main/reference/storage_zfs/), [storage volumes](https://linuxcontainers.org/incus/docs/main/howto/storage_volumes/).
- IncusOS: [introduction](https://linuxcontainers.org/incus-os/introduction/), [Incus application](https://linuxcontainers.org/incus-os/docs/main/reference/applications/incus/), [storage](https://linuxcontainers.org/incus-os/docs/main/reference/system/storage/).
- Coder: [architecture](https://coder.com/docs/install/plan/architecture/), [external authentication](https://coder.com/docs/admin/external-auth).
