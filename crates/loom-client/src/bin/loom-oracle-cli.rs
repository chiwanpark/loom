use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use loom_client::oracle_proto::{Device, Event, Pairing, PairingLink, RelayHost};
use loom_client::{AccessToken, ClientIdentity, OracleClient, OracleEndpoint, pair_oracle};
use loom_config::{Dirs, HumanDuration};
use serde::{Deserialize, Serialize};

const STATE_ENV: &str = "LOOM_ORACLE_CLI_STATE";
const IDENTITY_FILE: &str = "identity.json";
const ORACLE_FILE: &str = "oracle.json";
const CLIENT_NAME: &str = "loom-oracle-cli";

#[derive(Parser)]
#[command(
    name = "loom-oracle-cli",
    version = loom_version::VERSION,
    about = "Test client for the control service of a Loom-Next oracle"
)]
struct Cli {
    #[arg(long, global = true, env = STATE_ENV)]
    state_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Pair {
        link: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, requires = "access_secret_env")]
        access_id_env: Option<String>,
        #[arg(long, requires = "access_id_env")]
        access_secret_env: Option<String>,
    },
    Info,
    Devices,
    RenameDevice {
        id: u64,
        label: String,
    },
    RevokeDevice {
        id: u64,
    },
    PushToken {
        #[arg(required_unless_present = "clear")]
        token: Option<String>,
        #[arg(long, conflicts_with = "token")]
        clear: bool,
    },
    PairDevice {
        #[arg(long, value_parser = parse_duration)]
        ttl: Option<Duration>,
        #[arg(long)]
        no_qr: bool,
    },
    Pairings,
    CancelPairing {
        id: String,
    },
    RelayHosts,
    AddRelayHost {
        host_id: String,
        label: String,
    },
    RemoveRelayHost {
        host_id: String,
    },
    Watch {
        #[arg(long)]
        count: Option<usize>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedOracle {
    url: String,
    host_id: String,
    host_key: String,
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_id_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_secret_env: Option<String>,
}

impl SavedOracle {
    fn access(&self) -> anyhow::Result<Option<AccessToken>> {
        match (&self.access_id_env, &self.access_secret_env) {
            (Some(id), Some(secret)) => AccessToken::from_env(id, secret)
                .map(Some)
                .map_err(anyhow::Error::msg),
            _ => Ok(None),
        }
    }

    fn endpoint(&self) -> anyhow::Result<OracleEndpoint> {
        Ok(OracleEndpoint::from_parts(
            &self.url,
            &self.host_id,
            &self.host_key,
            self.access()?,
        )?)
    }
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<HumanDuration>()
        .map(|d| d.as_duration())
        .map_err(|e| e.to_string())
}

fn default_label() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| loom_crypto::validate_label(h).is_ok())
        .unwrap_or_else(|| CLIENT_NAME.to_owned())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("loom-oracle-cli: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("loom-oracle-cli: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn state_dir(cli: &Cli) -> anyhow::Result<PathBuf> {
    let dirs = Dirs::from_env()?;
    Ok(match &cli.state_dir {
        Some(dir) => dirs.expand(dir),
        None => dirs.default_data_dir().join("oracle-cli"),
    })
}

fn load_saved(dir: &Path) -> anyhow::Result<SavedOracle> {
    let path = dir.join(ORACLE_FILE);
    let text = match std::fs::read(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("no oracle is paired; run `loom-oracle-cli pair <link>` first")
        }
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    serde_json::from_slice(&text).with_context(|| format!("cannot parse {}", path.display()))
}

fn identity(dir: &Path) -> anyhow::Result<ClientIdentity> {
    let path = dir.join(IDENTITY_FILE);
    ClientIdentity::load_or_create(&path)
        .with_context(|| format!("cannot read the key {}", path.display()))
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn time(unix: u64) -> String {
    let elapsed = now_unix().saturating_sub(unix);
    format!("{} ago", HumanDuration(Duration::from_secs(elapsed)))
}

fn print_devices(devices: &[Device], me: u64) {
    for device in devices {
        let mut marks = Vec::new();
        if device.id == me {
            marks.push("this device".to_owned());
        }
        if device.connected {
            marks.push("connected".to_owned());
        } else if let Some(seen) = device.last_seen {
            marks.push(format!("seen {}", time(seen)));
        }
        if device.push {
            marks.push("push".to_owned());
        }
        println!("{}\t{}\t{}", device.id, device.label, marks.join(", "));
    }
}

fn print_pairing(pairing: &Pairing) {
    let left = pairing.expires_at.saturating_sub(now_unix());
    println!(
        "{}\t{}\texpires in {}",
        pairing.id,
        pairing.role,
        HumanDuration(Duration::from_secs(left))
    );
}

fn print_relay_host(host: &RelayHost) {
    let agents = if host.agents.is_empty() {
        "offline".to_owned()
    } else {
        host.agents.join(", ")
    };
    println!("{}\t{}\t{}", host.host_id, host.label, agents);
}

fn print_link(link: &PairingLink, qr: bool) {
    if qr && let Ok(code) = qrcode::QrCode::new(link.link.as_bytes()) {
        let image = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{image}");
    }
    println!("{}", link.link);
    let ttl = link
        .pairing
        .expires_at
        .saturating_sub(link.pairing.created_at);
    println!(
        "This link pairs one device. It works once and expires in {}.",
        HumanDuration(Duration::from_secs(ttl))
    );
}

fn print_event(event: &Event) {
    match event {
        Event::DeviceChanged { device } => println!(
            "device {} {}{}",
            device.id,
            device.label,
            if device.connected { " (connected)" } else { "" }
        ),
        Event::DeviceRemoved { id } => println!("device {id} removed"),
        Event::PairingChanged { pairing } => println!("pairing {} created", pairing.id),
        Event::PairingRemoved { id, used: true } => println!("pairing {id} used"),
        Event::PairingRemoved { id, used: false } => println!("pairing {id} removed"),
        Event::RelayHostChanged { relay_host } => {
            println!("relay host {} {}", relay_host.host_id, relay_host.label)
        }
        Event::RelayHostRemoved { host_id } => println!("relay host {host_id} removed"),
        Event::EventsLost => println!("some events were lost"),
        Event::Unknown => println!("an unknown event"),
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let dir = state_dir(&cli)?;
    let json = cli.json;
    if let Command::Pair {
        link,
        label,
        access_id_env,
        access_secret_env,
    } = cli.command
    {
        let saved = SavedOracle {
            url: String::new(),
            host_id: String::new(),
            host_key: String::new(),
            label: label.unwrap_or_else(default_label),
            access_id_env,
            access_secret_env,
        };
        let identity = identity(&dir)?;
        let endpoint = pair_oracle(&link, &identity, &saved.label, saved.access()?).await?;
        let saved = SavedOracle {
            url: endpoint.url.clone(),
            host_id: endpoint.host.to_string(),
            host_key: endpoint.host_key_text(),
            ..saved
        };
        let path = dir.join(ORACLE_FILE);
        std::fs::write(&path, serde_json::to_vec_pretty(&saved)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        if json {
            return print_json(&saved);
        }
        println!("paired with the oracle {} as {}", saved.url, saved.label);
        return Ok(());
    }

    let saved = load_saved(&dir)?;
    let (client, mut events) =
        OracleClient::connect(&saved.endpoint()?, &identity(&dir)?, CLIENT_NAME)
            .await
            .with_context(|| format!("cannot connect to the oracle {}", saved.url))?;
    match cli.command {
        Command::Pair { .. } => unreachable!(),
        Command::Info => {
            let info = client.info().await?;
            if json {
                return print_json(&serde_json::json!({
                    "oracle": info,
                    "peer": client.hello().peer,
                }));
            }
            let peer = &client.hello().peer;
            println!("oracle {} (protocol {})", info.version, info.protocol);
            println!("url {}", info.url);
            println!("host {}", info.host_id);
            println!("peer {} {} ({})", peer.id, peer.label, peer.role);
        }
        Command::Devices => {
            let devices = client.devices().await?;
            if json {
                return print_json(&devices);
            }
            print_devices(&devices, client.hello().peer.id);
        }
        Command::RenameDevice { id, label } => {
            let device = client.rename_device(id, &label).await?;
            if json {
                return print_json(&device);
            }
            println!("device {} is now {}", device.id, device.label);
        }
        Command::RevokeDevice { id } => {
            client.revoke_device(id).await?;
            if !json {
                println!("revoked device {id}");
            }
        }
        Command::PushToken { token, clear: _ } => {
            client.set_push_token(token.as_deref()).await?;
            if !json {
                match token {
                    Some(_) => println!("set the push token"),
                    None => println!("cleared the push token"),
                }
            }
        }
        Command::PairDevice { ttl, no_qr } => {
            let link = client.create_device_pairing(ttl).await?;
            if json {
                return print_json(&link);
            }
            print_link(&link, !no_qr);
        }
        Command::Pairings => {
            let pairings = client.pairings().await?;
            if json {
                return print_json(&pairings);
            }
            pairings.iter().for_each(print_pairing);
        }
        Command::CancelPairing { id } => {
            client.cancel_pairing(&id).await?;
            if !json {
                println!("cancelled pairing {id}");
            }
        }
        Command::RelayHosts => {
            let hosts = client.relay_hosts().await?;
            if json {
                return print_json(&hosts);
            }
            hosts.iter().for_each(print_relay_host);
        }
        Command::AddRelayHost { host_id, label } => {
            let host = client.add_relay_host(&host_id, &label).await?;
            if json {
                return print_json(&host);
            }
            print_relay_host(&host);
        }
        Command::RemoveRelayHost { host_id } => {
            client.remove_relay_host(&host_id).await?;
            if !json {
                println!("removed relay host {host_id}");
            }
        }
        Command::Watch { count } => {
            client.subscribe().await?;
            if json {
                print_json(&serde_json::json!({"type": "subscribed"}))?;
            } else {
                println!("watching the events of {}", saved.url);
            }
            let mut seen = 0;
            while count.is_none_or(|count| seen < count) {
                let event = tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    event = events.recv() => event,
                };
                let Some(event) = event else {
                    bail!("the oracle closed the connection");
                };
                if json {
                    print_json(&event)?;
                } else {
                    print_event(&event);
                }
                seen += 1;
            }
        }
    }
    Ok(())
}
