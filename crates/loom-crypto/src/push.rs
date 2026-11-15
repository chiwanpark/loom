//! Notifications for a paired client, sealed so that only that client can
//! read them and it knows they come from its host.
//!
//! A notification goes through the relay and a push service such as
//! Firebase Cloud Messaging, which should learn nothing but its size. It is
//! one Noise message of the `K` pattern: the host and the client know each
//! other's static key from pairing, so the client can check the sender, and
//! a fresh ephemeral key makes every message different.

use crate::error::CryptoError;
use crate::identity::{ClientIdentity, HostIdentity};

const PUSH_PARAMS: &str = "Noise_K_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"loom-push/1";
/// The largest notification; push services take a few kilobytes at most.
pub const MAX_PUSH_PLAINTEXT: usize = 2048;
/// What sealing adds: the ephemeral key and the tag.
pub const PUSH_OVERHEAD: usize = 32 + 16;

/// Seals `plaintext` from `host` for the client with the static key `client`.
pub fn seal_push(
    host: &HostIdentity,
    client: &[u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if plaintext.len() > MAX_PUSH_PLAINTEXT {
        return Err(CryptoError::TooLarge(plaintext.len()));
    }
    let mut state = snow::Builder::new(PUSH_PARAMS.parse()?)
        .local_private_key(host.static_private())?
        .remote_public_key(client)?
        .prologue(PROLOGUE)?
        .build_initiator()?;
    let mut out = vec![0u8; plaintext.len() + PUSH_OVERHEAD];
    let len = state.write_message(plaintext, &mut out)?;
    out.truncate(len);
    Ok(out)
}

/// Opens a notification sealed for `client` by the host with the static key
/// `host_static`; fails when it was not, or was changed on the way.
pub fn open_push(
    client: &ClientIdentity,
    host_static: &[u8; 32],
    sealed: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() > MAX_PUSH_PLAINTEXT + PUSH_OVERHEAD {
        return Err(CryptoError::TooLarge(sealed.len()));
    }
    let mut state = snow::Builder::new(PUSH_PARAMS.parse()?)
        .local_private_key(client.static_private())?
        .remote_public_key(host_static)?
        .prologue(PROLOGUE)?
        .build_responder()?;
    let mut out = vec![0u8; sealed.len()];
    let len = state.read_message(sealed, &mut out)?;
    out.truncate(len);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> (tempfile::TempDir, HostIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let host = HostIdentity::load_or_create(dir.path()).unwrap();
        (dir, host)
    }

    #[test]
    fn only_the_client_opens_what_its_host_sealed() {
        let (_dir, host) = host();
        let (_other_dir, other_host) = self::host();
        let client = ClientIdentity::generate().unwrap();
        let stranger = ClientIdentity::generate().unwrap();

        let sealed = seal_push(&host, &client.public_key(), b"approval needed").unwrap();
        assert_eq!(sealed.len(), b"approval needed".len() + PUSH_OVERHEAD);
        let again = seal_push(&host, &client.public_key(), b"approval needed").unwrap();
        assert_ne!(sealed, again, "every message has its own ephemeral key");
        assert_eq!(
            open_push(&client, &host.static_public(), &sealed).unwrap(),
            b"approval needed"
        );

        // Another client cannot read it, and it does not pass for another host's.
        assert!(open_push(&stranger, &host.static_public(), &sealed).is_err());
        assert!(open_push(&client, &other_host.static_public(), &sealed).is_err());
        // Nor does a changed message.
        let mut changed = sealed.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(open_push(&client, &host.static_public(), &changed).is_err());
        assert!(open_push(&client, &host.static_public(), &sealed[..10]).is_err());
    }

    #[test]
    fn notifications_have_a_size_limit() {
        let (_dir, host) = host();
        let client = ClientIdentity::generate().unwrap();
        let largest = vec![b'x'; MAX_PUSH_PLAINTEXT];
        let sealed = seal_push(&host, &client.public_key(), &largest).unwrap();
        assert_eq!(
            open_push(&client, &host.static_public(), &sealed).unwrap(),
            largest
        );
        assert!(matches!(
            seal_push(&host, &client.public_key(), &[0; MAX_PUSH_PLAINTEXT + 1]),
            Err(CryptoError::TooLarge(_))
        ));
    }
}
