//! The Tokio runtime the transports run on. Kotlin calls the core from its
//! own threads and coroutines, so every async call hops onto this runtime
//! and the caller waits for the result.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        install_crypto();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("loom-core")
            .enable_all()
            .build()
            .expect("could not start the Tokio runtime")
    })
}

/// Starts `future` on the runtime.
pub fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> JoinHandle<T> {
    runtime().spawn(future)
}

/// Aborts the task when the caller stops waiting for it.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs `future` on the runtime and waits for it from any executor. When
/// Kotlin cancels its coroutine, UniFFI drops this future, which aborts the
/// task, so cancellation reaches the work itself.
pub async fn run<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> T {
    let mut task = AbortOnDrop(spawn(future));
    match (&mut task.0).await {
        Ok(value) => value,
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => panic!("the core's task was cancelled: {err}"),
    }
}

/// Makes ring rustls's crypto for the process. rustls picks one by itself
/// only when its features name exactly one, which the core's do not; without
/// this, the first wss:// connection panics.
pub fn install_crypto() {
    // An error means a provider is installed already, which is as good.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
