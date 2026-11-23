mod oracle;
mod seal;
mod session;
pub mod store;

use std::io;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use oracle::{
    CREDENTIALS_ENV, Oracle, RelayHosts, STORE_FILE, master_key_path, open_store, pairing_link,
    pairing_ttl, valid_branch, validate_environment,
};
pub use seal::{MasterKey, SealError, Sealer};
pub use store::{Store, StoreError};

pub async fn serve(
    oracle: Arc<Oracle>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> io::Result<()> {
    oracle.start(shutdown.clone());
    loom_relay::serve(oracle.relay().clone(), listener, shutdown).await
}

pub async fn serve_tls(
    oracle: Arc<Oracle>,
    listener: loom_relay::TlsListener,
    shutdown: CancellationToken,
) -> io::Result<()> {
    oracle.start(shutdown.clone());
    loom_relay::serve_tls(oracle.relay().clone(), listener, shutdown).await
}
