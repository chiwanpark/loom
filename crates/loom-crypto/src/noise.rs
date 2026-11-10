use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use snow::{HandshakeState, TransportState};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::error::CryptoError;
use crate::identity::{ClientIdentity, HostIdentity};

pub(crate) const IK_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub(crate) const XX_PARAMS: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
const VERSION: u8 = 1;
const MODE_CONNECT: u8 = 0;
const MODE_PAIR: u8 = 1;
const STATUS_OK: u8 = 0;
const STATUS_REJECTED: u8 = 1;
const MAX_MESSAGE: usize = 65_535;
const TAG_LEN: usize = 16;
const CHUNK: usize = MAX_MESSAGE - TAG_LEN - 1;
const FINAL: u8 = 1;
const MORE: u8 = 0;
const DEFAULT_REKEY_INTERVAL: u64 = 1 << 16;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_PLAINTEXT_FRAME: usize = 16 * 1024 * 1024 + 64;

pub struct PacketChannel {
    pub tx: mpsc::Sender<Vec<u8>>,
    pub rx: mpsc::Receiver<Vec<u8>>,
}

impl PacketChannel {
    pub fn pair(capacity: usize) -> (Self, Self) {
        let (a_tx, a_rx) = mpsc::channel(capacity);
        let (b_tx, b_rx) = mpsc::channel(capacity);
        (Self { tx: a_tx, rx: b_rx }, Self { tx: b_tx, rx: a_rx })
    }

    async fn send(&self, packet: Vec<u8>) -> Result<(), CryptoError> {
        self.tx.send(packet).await.map_err(|_| CryptoError::Closed)
    }

    async fn recv(&mut self) -> Result<Vec<u8>, CryptoError> {
        self.rx.recv().await.ok_or(CryptoError::Closed)
    }
}

pub enum Mode<'a> {
    Connect,
    Pair {
        secret: &'a [u8; 32],
        label: &'a str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub pubkey: [u8; 32],
    pub label: String,
}

pub trait Responder: Send + Sync {
    fn authorize(&self, client: &[u8; 32]) -> Result<String, String>;
    fn take_pairing(&self, id: &str) -> Option<Zeroizing<[u8; 32]>>;
    fn paired(&self, client: [u8; 32], label: &str) -> Result<(), String>;
}

fn prologue(mode: u8, pairing_id: &str) -> Vec<u8> {
    let mut prologue = b"loom-noise/1".to_vec();
    prologue.push(mode);
    prologue.extend_from_slice(pairing_id.as_bytes());
    prologue
}

fn write(state: &mut HandshakeState, payload: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut buf = vec![0u8; MAX_MESSAGE];
    let len = state.write_message(payload, &mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

fn read(state: &mut HandshakeState, message: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut buf = vec![0u8; MAX_MESSAGE];
    let len = state.read_message(message, &mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

fn remote_static(state: &HandshakeState) -> Result<[u8; 32], CryptoError> {
    state
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| CryptoError::Invalid("the peer sent no static key".into()))
}

fn status(packet: &[u8]) -> Result<&[u8], CryptoError> {
    match packet.split_first() {
        Some((&STATUS_OK, rest)) => Ok(rest),
        Some((&STATUS_REJECTED, reason)) => Err(CryptoError::Rejected(
            String::from_utf8_lossy(reason).into_owned(),
        )),
        _ => Err(CryptoError::Invalid("malformed handshake packet".into())),
    }
}

fn with_status(code: u8, body: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(body.len() + 1);
    packet.push(code);
    packet.extend_from_slice(body);
    packet
}

pub async fn initiate(
    channel: PacketChannel,
    identity: &ClientIdentity,
    host_static: &[u8; 32],
    mode: Mode<'_>,
) -> Result<SecureChannel, CryptoError> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        initiate_inner(channel, identity, host_static, mode),
    )
    .await
    .map_err(|_| CryptoError::Invalid("the handshake timed out".into()))?
}

async fn initiate_inner(
    mut channel: PacketChannel,
    identity: &ClientIdentity,
    host_static: &[u8; 32],
    mode: Mode<'_>,
) -> Result<SecureChannel, CryptoError> {
    let (mode_byte, pairing_id) = match &mode {
        Mode::Connect => (MODE_CONNECT, String::new()),
        Mode::Pair { secret, .. } => (MODE_PAIR, crate::pairing::pairing_id(secret)),
    };
    let prologue = prologue(mode_byte, &pairing_id);
    let mut header = vec![VERSION, mode_byte, pairing_id.len() as u8];
    header.extend_from_slice(pairing_id.as_bytes());
    let state = match mode {
        Mode::Connect => {
            let mut state = snow::Builder::new(IK_PARAMS.parse()?)
                .local_private_key(identity.static_private())?
                .remote_public_key(host_static)?
                .prologue(&prologue)?
                .build_initiator()?;
            header.extend(write(&mut state, &[])?);
            channel.send(header).await?;
            let reply = channel.recv().await?;
            read(&mut state, status(&reply)?)?;
            state
        }
        Mode::Pair { secret, label } => {
            let mut state = snow::Builder::new(XX_PARAMS.parse()?)
                .local_private_key(identity.static_private())?
                .psk(3, secret)?
                .prologue(&prologue)?
                .build_initiator()?;
            header.extend(write(&mut state, &[])?);
            channel.send(header).await?;
            let reply = channel.recv().await?;
            read(&mut state, status(&reply)?)?;
            if &remote_static(&state)? != host_static {
                return Err(CryptoError::HostKeyMismatch);
            }
            channel.send(write(&mut state, label.as_bytes())?).await?;
            state
        }
    };
    if &remote_static(&state)? != host_static {
        return Err(CryptoError::HostKeyMismatch);
    }
    let mut secure = SecureChannel::new(state.into_transport_mode()?, channel, *host_static);
    match secure.receiver.recv().await? {
        Some(ack) => {
            status(&ack)?;
        }
        None => return Err(CryptoError::Closed),
    }
    Ok(secure)
}

pub async fn respond(
    channel: PacketChannel,
    host: &HostIdentity,
    responder: &dyn Responder,
) -> Result<(SecureChannel, Peer), CryptoError> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, respond_inner(channel, host, responder))
        .await
        .map_err(|_| CryptoError::Invalid("the handshake timed out".into()))?
}

async fn reject(channel: &PacketChannel, reason: String) -> CryptoError {
    let _ = channel
        .send(with_status(STATUS_REJECTED, reason.as_bytes()))
        .await;
    CryptoError::Rejected(reason)
}

async fn respond_inner(
    mut channel: PacketChannel,
    host: &HostIdentity,
    responder: &dyn Responder,
) -> Result<(SecureChannel, Peer), CryptoError> {
    let first = channel.recv().await?;
    let malformed = || CryptoError::Invalid("malformed handshake header".into());
    let [version, mode, id_len, rest @ ..] = first.as_slice() else {
        return Err(malformed());
    };
    if *version != VERSION {
        return Err(reject(&channel, format!("unsupported handshake version {version}")).await);
    }
    let id_len = usize::from(*id_len);
    if rest.len() < id_len {
        return Err(malformed());
    }
    let (id, message) = rest.split_at(id_len);
    let pairing_id = std::str::from_utf8(id).map_err(|_| malformed())?.to_owned();
    let prologue = prologue(*mode, &pairing_id);
    match *mode {
        MODE_CONNECT => {
            let mut state = snow::Builder::new(IK_PARAMS.parse()?)
                .local_private_key(host.static_private())?
                .prologue(&prologue)?
                .build_responder()?;
            read(&mut state, message)?;
            let client = remote_static(&state)?;
            let label = match responder.authorize(&client) {
                Ok(label) => label,
                Err(reason) => return Err(reject(&channel, reason).await),
            };
            let reply = write(&mut state, &[])?;
            channel.send(with_status(STATUS_OK, &reply)).await?;
            let secure = SecureChannel::new(state.into_transport_mode()?, channel, client);
            secure.sender.send(&[STATUS_OK]).await?;
            Ok((
                secure,
                Peer {
                    pubkey: client,
                    label,
                },
            ))
        }
        MODE_PAIR => {
            let Some(secret) = responder.take_pairing(&pairing_id) else {
                return Err(
                    reject(&channel, "the pairing code is unknown or expired".into()).await,
                );
            };
            let mut state = snow::Builder::new(XX_PARAMS.parse()?)
                .local_private_key(host.static_private())?
                .psk(3, &secret)?
                .prologue(&prologue)?
                .build_responder()?;
            read(&mut state, message)?;
            let reply = write(&mut state, &[])?;
            channel.send(with_status(STATUS_OK, &reply)).await?;
            let third = channel.recv().await?;
            let label = read(&mut state, &third)?;
            let label = String::from_utf8(label)
                .map_err(|_| CryptoError::Invalid("the client label is not UTF-8".into()))?;
            let client = remote_static(&state)?;
            let secure = SecureChannel::new(state.into_transport_mode()?, channel, client);
            if let Err(reason) = responder.paired(client, &label) {
                let _ = secure
                    .sender
                    .send(&with_status(STATUS_REJECTED, reason.as_bytes()))
                    .await;
                return Err(CryptoError::Rejected(reason));
            }
            secure.sender.send(&[STATUS_OK]).await?;
            Ok((
                secure,
                Peer {
                    pubkey: client,
                    label,
                },
            ))
        }
        other => Err(reject(&channel, format!("unknown handshake mode {other}")).await),
    }
}

struct Cipher {
    state: TransportState,
    rekey_interval: u64,
}

fn lock(cipher: &Mutex<Cipher>) -> MutexGuard<'_, Cipher> {
    cipher.lock().unwrap_or_else(|e| e.into_inner())
}

impl std::fmt::Debug for SecureChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureChannel").finish_non_exhaustive()
    }
}

pub struct SecureChannel {
    pub sender: SecureSender,
    pub receiver: SecureReceiver,
    remote_static: [u8; 32],
}

pub struct SecureSender {
    cipher: Arc<Mutex<Cipher>>,
    tx: mpsc::Sender<Vec<u8>>,
}

pub struct SecureReceiver {
    cipher: Arc<Mutex<Cipher>>,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl SecureChannel {
    fn new(state: TransportState, channel: PacketChannel, remote_static: [u8; 32]) -> Self {
        let cipher = Arc::new(Mutex::new(Cipher {
            state,
            rekey_interval: DEFAULT_REKEY_INTERVAL,
        }));
        Self {
            sender: SecureSender {
                cipher: cipher.clone(),
                tx: channel.tx,
            },
            receiver: SecureReceiver {
                cipher,
                rx: channel.rx,
            },
            remote_static,
        }
    }

    pub fn remote_static(&self) -> [u8; 32] {
        self.remote_static
    }

    pub fn split(self) -> (SecureSender, SecureReceiver) {
        (self.sender, self.receiver)
    }

    #[cfg(test)]
    fn set_rekey_interval(&self, interval: u64) {
        lock(&self.sender.cipher).rekey_interval = interval;
    }
}

impl SecureSender {
    pub async fn send(&self, frame: &[u8]) -> Result<(), CryptoError> {
        if frame.len() > MAX_PLAINTEXT_FRAME {
            return Err(CryptoError::TooLarge(frame.len()));
        }
        let packets = {
            let mut cipher = lock(&self.cipher);
            let mut packets = Vec::with_capacity(frame.len() / CHUNK + 1);
            let mut chunks = frame.chunks(CHUNK).peekable();
            if chunks.peek().is_none() {
                packets.push(encrypt(&mut cipher, FINAL, &[])?);
            }
            while let Some(chunk) = chunks.next() {
                let flag = if chunks.peek().is_some() { MORE } else { FINAL };
                packets.push(encrypt(&mut cipher, flag, chunk)?);
            }
            packets
        };
        for packet in packets {
            self.tx
                .send(packet)
                .await
                .map_err(|_| CryptoError::Closed)?;
        }
        Ok(())
    }
}

fn encrypt(cipher: &mut Cipher, flag: u8, chunk: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let nonce = cipher.state.sending_nonce();
    if nonce > 0 && nonce.is_multiple_of(cipher.rekey_interval) {
        cipher.state.rekey_outgoing();
    }
    let mut plain = Vec::with_capacity(chunk.len() + 1);
    plain.push(flag);
    plain.extend_from_slice(chunk);
    let mut out = vec![0u8; plain.len() + TAG_LEN];
    let len = cipher.state.write_message(&plain, &mut out)?;
    out.truncate(len);
    Ok(out)
}

impl SecureReceiver {
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>, CryptoError> {
        let mut frame = Vec::new();
        loop {
            let Some(packet) = self.rx.recv().await else {
                return if frame.is_empty() {
                    Ok(None)
                } else {
                    Err(CryptoError::Closed)
                };
            };
            let plain = {
                let mut cipher = lock(&self.cipher);
                let nonce = cipher.state.receiving_nonce();
                if nonce > 0 && nonce.is_multiple_of(cipher.rekey_interval) {
                    cipher.state.rekey_incoming();
                }
                let mut out = vec![0u8; packet.len()];
                let len = cipher.state.read_message(&packet, &mut out)?;
                out.truncate(len);
                out
            };
            let Some((&flag, body)) = plain.split_first() else {
                return Err(CryptoError::Invalid("empty secure packet".into()));
            };
            if frame.len() + body.len() > MAX_PLAINTEXT_FRAME {
                return Err(CryptoError::TooLarge(frame.len() + body.len()));
            }
            frame.extend_from_slice(body);
            match flag {
                FINAL => return Ok(Some(frame)),
                MORE => {}
                _ => return Err(CryptoError::Invalid("bad fragment flag".into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::pairing::pairing_id;

    #[derive(Default)]
    struct TestResponder {
        clients: Mutex<HashMap<[u8; 32], String>>,
        pairings: Mutex<HashMap<String, [u8; 32]>>,
    }

    impl Responder for TestResponder {
        fn authorize(&self, client: &[u8; 32]) -> Result<String, String> {
            self.clients
                .lock()
                .unwrap()
                .get(client)
                .cloned()
                .ok_or_else(|| "this client is not authorized".to_owned())
        }

        fn take_pairing(&self, id: &str) -> Option<Zeroizing<[u8; 32]>> {
            self.pairings.lock().unwrap().remove(id).map(Zeroizing::new)
        }

        fn paired(&self, client: [u8; 32], label: &str) -> Result<(), String> {
            self.clients
                .lock()
                .unwrap()
                .insert(client, label.to_owned());
            Ok(())
        }
    }

    fn host() -> (tempfile::TempDir, HostIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let host = HostIdentity::load_or_create(dir.path()).unwrap();
        (dir, host)
    }

    async fn handshake(
        host: &HostIdentity,
        responder: &TestResponder,
        client: &ClientIdentity,
        host_static: [u8; 32],
        mode: Mode<'_>,
    ) -> (
        Result<SecureChannel, CryptoError>,
        Result<(SecureChannel, Peer), CryptoError>,
    ) {
        let (a, b) = PacketChannel::pair(64);
        tokio::join!(
            initiate(a, client, &host_static, mode),
            respond(b, host, responder)
        )
    }

    #[tokio::test]
    async fn pairing_then_connecting() {
        let (_dir, host) = host();
        let responder = TestResponder::default();
        let client = ClientIdentity::generate().unwrap();

        let (c, r) = handshake(
            &host,
            &responder,
            &client,
            host.static_public(),
            Mode::Connect,
        )
        .await;
        assert!(matches!(c, Err(CryptoError::Rejected(ref m)) if m.contains("not authorized")));
        assert!(r.is_err());

        let secret = [5u8; 32];
        responder
            .pairings
            .lock()
            .unwrap()
            .insert(pairing_id(&secret), secret);
        let (c, r) = handshake(
            &host,
            &responder,
            &client,
            host.static_public(),
            Mode::Pair {
                secret: &secret,
                label: "laptop",
            },
        )
        .await;
        c.unwrap();
        let (_, peer) = r.unwrap();
        assert_eq!(peer.label, "laptop");
        assert_eq!(peer.pubkey, client.public_key());

        let (c, r) = handshake(
            &host,
            &responder,
            &client,
            host.static_public(),
            Mode::Pair {
                secret: &secret,
                label: "again",
            },
        )
        .await;
        assert!(matches!(c, Err(CryptoError::Rejected(ref m)) if m.contains("unknown or expired")));
        assert!(r.is_err());

        let (c, r) = handshake(
            &host,
            &responder,
            &client,
            host.static_public(),
            Mode::Connect,
        )
        .await;
        let client_channel = c.unwrap();
        let (host_channel, peer) = r.unwrap();
        assert_eq!(peer.label, "laptop");
        assert_eq!(client_channel.remote_static(), host.static_public());

        let (sender, _keep) = client_channel.split();
        let (_hs, mut receiver) = host_channel.split();
        let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let sent = big.clone();
        let task = tokio::spawn(async move {
            sender.send(&sent).await.unwrap();
            sender.send(&[]).await.unwrap();
            sender.send(b"tail").await.unwrap();
        });
        assert_eq!(receiver.recv().await.unwrap().unwrap(), big);
        assert_eq!(receiver.recv().await.unwrap().unwrap(), b"");
        assert_eq!(receiver.recv().await.unwrap().unwrap(), b"tail");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_keys_and_secrets_fail() {
        let (_dir, host) = host();
        let responder = TestResponder::default();
        let client = ClientIdentity::generate().unwrap();
        responder
            .clients
            .lock()
            .unwrap()
            .insert(client.public_key(), "c".into());
        let (c, r) = handshake(&host, &responder, &client, [9; 32], Mode::Connect).await;
        assert!(c.is_err());
        assert!(r.is_err());

        let secret = [5u8; 32];
        responder
            .pairings
            .lock()
            .unwrap()
            .insert(pairing_id(&secret), [6u8; 32]);
        let (c, r) = handshake(
            &host,
            &responder,
            &client,
            host.static_public(),
            Mode::Pair {
                secret: &secret,
                label: "x",
            },
        )
        .await;
        assert!(c.is_err());
        assert!(r.is_err());

        let (other_dir, other) = {
            let dir = tempfile::tempdir().unwrap();
            let host = HostIdentity::load_or_create(dir.path()).unwrap();
            (dir, host)
        };
        responder
            .pairings
            .lock()
            .unwrap()
            .insert(pairing_id(&secret), secret);
        let (c, _r) = handshake(
            &host,
            &responder,
            &client,
            other.static_public(),
            Mode::Pair {
                secret: &secret,
                label: "x",
            },
        )
        .await;
        assert!(matches!(c, Err(CryptoError::HostKeyMismatch)), "{c:?}");
        drop(other_dir);
    }

    #[tokio::test]
    async fn rekeys_in_both_directions() {
        let (_dir, host) = host();
        let responder = TestResponder::default();
        let client = ClientIdentity::generate().unwrap();
        responder
            .clients
            .lock()
            .unwrap()
            .insert(client.public_key(), "c".into());
        let (c, r) = handshake(
            &host,
            &responder,
            &client,
            host.static_public(),
            Mode::Connect,
        )
        .await;
        let c = c.unwrap();
        let (h, _) = r.unwrap();
        c.set_rekey_interval(3);
        h.set_rekey_interval(3);
        let (cs, mut cr) = c.split();
        let (hs, mut hr) = h.split();
        for i in 0..20u8 {
            cs.send(&[i; 10]).await.unwrap();
            assert_eq!(hr.recv().await.unwrap().unwrap(), [i; 10]);
            hs.send(&[i; 3]).await.unwrap();
            assert_eq!(cr.recv().await.unwrap().unwrap(), [i; 3]);
        }
        drop(cs);
        assert!(hr.recv().await.unwrap().is_none());
    }

    /// Frame sizes that exercise fragmentation: small, around one chunk, and
    /// around two chunks.
    fn frame_len() -> impl proptest::strategy::Strategy<Value = usize> {
        use proptest::prelude::*;
        prop_oneof![
            0..300usize,
            (CHUNK - 2)..(CHUNK + 3),
            (2 * CHUNK - 2)..(2 * CHUNK + 3),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        /// Any sequence of frames arrives intact and in order, in both directions,
        /// whatever the fragmentation and however often the keys rotate.
        #[test]
        fn frames_roundtrip_through_fragmentation_and_rekeying(
            lens in proptest::collection::vec(frame_len(), 1..6),
            seed in proptest::prelude::any::<u8>(),
            rekey in 1u64..8,
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (_dir, host) = host();
                let responder = TestResponder::default();
                let client = ClientIdentity::generate().unwrap();
                responder
                    .clients
                    .lock()
                    .unwrap()
                    .insert(client.public_key(), "test".into());
                let (c, r) =
                    handshake(&host, &responder, &client, host.static_public(), Mode::Connect)
                        .await;
                let c = c.unwrap();
                let (h, _) = r.unwrap();
                c.set_rekey_interval(rekey);
                h.set_rekey_interval(rekey);
                let (cs, mut cr) = c.split();
                let (hs, mut hr) = h.split();
                let frames: Vec<Vec<u8>> = lens
                    .iter()
                    .enumerate()
                    .map(|(i, &len)| {
                        (0..len)
                            .map(|j| (j as u8).wrapping_mul(31) ^ seed ^ i as u8)
                            .collect()
                    })
                    .collect();
                for frame in &frames {
                    let (sent, received) = tokio::join!(cs.send(frame), hr.recv());
                    sent.unwrap();
                    assert_eq!(&received.unwrap().unwrap(), frame);
                    let (sent, received) = tokio::join!(hs.send(frame), cr.recv());
                    sent.unwrap();
                    assert_eq!(&received.unwrap().unwrap(), frame);
                }
            });
        }
    }
}
