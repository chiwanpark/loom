//! The TCP ports that programs listen on, read from `/proc` on Linux.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use loom_proto::{ApiError, ErrorCode, ListeningPort};

/// The state of a listening socket in `/proc/net/tcp`.
const LISTEN: &str = "0A";

/// A listening socket from `/proc/net/tcp` or `/proc/net/tcp6`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listener {
    address: String,
    port: u16,
    inode: u64,
}

/// The listening sockets in a `/proc/net/tcp` or `/proc/net/tcp6` table.
fn parse_table(text: &str, v6: bool) -> Vec<Listener> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != LISTEN {
                return None;
            }
            let (address, port) = fields[1].split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let address = if v6 {
                parse_v6(address)?.to_string()
            } else {
                parse_v4(address)?.to_string()
            };
            let inode = fields[9].parse().ok()?;
            Some(Listener {
                address,
                port,
                inode,
            })
        })
        .collect()
}

/// An IPv4 address as the kernel prints it: four bytes in host order, in hex.
fn parse_v4(hex: &str) -> Option<Ipv4Addr> {
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(value.to_ne_bytes()))
}

/// An IPv6 address as the kernel prints it: four words in host order.
fn parse_v6(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in bytes.chunks_mut(4).enumerate() {
        let word = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok()?;
        chunk.copy_from_slice(&word.to_ne_bytes());
    }
    Some(Ipv6Addr::from(bytes))
}

/// The process of each socket inode that the agent may look at: those of
/// its own user, as `/proc/<pid>/fd` of other users cannot be read.
fn socket_owners(proc: &Path) -> HashMap<u64, (u32, String)> {
    let mut owners = HashMap::new();
    let Ok(entries) = std::fs::read_dir(proc) else {
        return owners;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let name = std::fs::read_to_string(entry.path().join("comm"))
            .map(|name| name.trim().to_owned())
            .unwrap_or_default();
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
                .and_then(|t| t.parse::<u64>().ok())
            else {
                continue;
            };
            owners.entry(inode).or_insert_with(|| (pid, name.clone()));
        }
    }
    owners
}

/// The ports that programs of the agent's user listen on, by port number,
/// one entry per port: the one on IPv4 when a program listens on both.
pub fn listening_ports_in(proc: &Path) -> Vec<ListeningPort> {
    let mut listeners = Vec::new();
    for (table, v6) in [("net/tcp", false), ("net/tcp6", true)] {
        if let Ok(text) = std::fs::read_to_string(proc.join(table)) {
            listeners.extend(parse_table(&text, v6));
        }
    }
    let owners = socket_owners(proc);
    let mut seen = HashSet::new();
    let mut ports: Vec<ListeningPort> = listeners
        .into_iter()
        .filter_map(|listener| {
            let (pid, name) = owners.get(&listener.inode)?;
            seen.insert(listener.port).then(|| ListeningPort {
                port: listener.port,
                address: listener.address,
                process: Some(name.clone()).filter(|name| !name.is_empty()),
                pid: Some(*pid),
            })
        })
        .collect();
    ports.sort_by_key(|port| port.port);
    ports
}

/// The ports that programs of the agent's user listen on.
pub async fn listening_ports() -> Result<Vec<ListeningPort>, ApiError> {
    if !cfg!(target_os = "linux") {
        return Err(ApiError::new(
            ErrorCode::Unsupported,
            "the agent finds listening ports on Linux only",
        ));
    }
    tokio::task::spawn_blocking(|| listening_ports_in(Path::new("/proc")))
        .await
        .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 101 1 0000000000000000 100 0 0 10 0
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 102 1 0000000000000000 100 0 0 10 0
   2: 0100007F:0BB8 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 103 1 0000000000000000 20 4 30 10 -1
";

    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 104 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:1388 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 105 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn listening_sockets_are_read_from_the_tables() {
        assert_eq!(
            parse_table(TCP, false),
            [
                Listener {
                    address: "127.0.0.1".into(),
                    port: 3000,
                    inode: 101
                },
                Listener {
                    address: "0.0.0.0".into(),
                    port: 8080,
                    inode: 102
                },
            ]
        );
        assert_eq!(
            parse_table(TCP6, true),
            [
                Listener {
                    address: "::".into(),
                    port: 8080,
                    inode: 104
                },
                Listener {
                    address: "::1".into(),
                    port: 5000,
                    inode: 105
                },
            ]
        );
    }

    #[test]
    fn ports_of_the_agents_user_are_listed_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ports = listening_ports_in(Path::new("/proc"));
        let found: Vec<_> = ports.iter().filter(|p| p.port == port).collect();
        assert_eq!(found.len(), 1, "{ports:?}");
        assert_eq!(found[0].address, "127.0.0.1");
        assert_eq!(found[0].pid, Some(std::process::id()));
        drop(listener);
        let ports = listening_ports_in(Path::new("/proc"));
        assert!(ports.iter().all(|p| p.port != port));
    }
}
