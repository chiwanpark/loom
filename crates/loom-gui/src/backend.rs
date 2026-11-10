use std::future::Future;
use std::sync::Arc;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt};
use loom_client::{Notification, Notifications};

#[derive(Clone)]
pub struct Backend {
    runtime: Arc<tokio::runtime::Runtime>,
}

impl Backend {
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("loom-backend")
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    pub fn run<F>(&self, future: F) -> impl Future<Output = Option<F::Output>> + use<F>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(future.await);
        });
        rx.map(Result::ok)
    }

    pub fn forward(
        &self,
        mut notifications: Notifications,
    ) -> mpsc::UnboundedReceiver<Notification> {
        let (tx, rx) = mpsc::unbounded();
        self.runtime.spawn(async move {
            while let Some(note) = notifications.recv().await {
                if tx.unbounded_send(note).is_err() {
                    break;
                }
            }
        });
        rx
    }
}

pub async fn next<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> Option<T> {
    rx.next().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_tokio_futures_from_other_executors() {
        let backend = Backend::new().unwrap();
        let value = futures::executor::block_on(backend.run(async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            42
        }));
        assert_eq!(value, Some(42));
    }
}
