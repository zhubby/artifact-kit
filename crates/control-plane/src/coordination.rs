//! Process-local connection ownership and wakeups backed by durable database work.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Remove this replica from scheduling while existing sessions may renew leases.
///
/// # Errors
/// Returns a database error if connection ownership cannot be cleared.
pub async fn drain(state: &crate::state::AppState) -> anyhow::Result<()> {
    state.coordination.drain();
    state.store.drain_owner(state.coordination.id).await
}

/// A fresh identity on every process start prevents an old process owning new sessions.
#[derive(Debug)]
pub struct Coordination {
    pub(crate) id: Uuid,
    pub(crate) wake: Notify,
    pub(crate) commands: Notify,
    pub(crate) stop: CancellationToken,
    // Leave database capacity for established sessions even at the minimum pool
    // budget. The permit covers authorization, session replacement and hello.
    pub(crate) admissions: Arc<Semaphore>,
    pub(crate) telemetry_client: tokio::sync::OnceCell<reqwest::Client>,
    draining: AtomicBool,
}

impl Default for Coordination {
    fn default() -> Self {
        // The smallest supported business pool has eight connections.
        Self::new(8)
    }
}

impl Coordination {
    /// Limit session initialization to half the pool, capped at sixteen callers.
    pub fn new(database_connections: u32) -> Self {
        Self {
            id: Uuid::new_v4(),
            wake: Notify::new(),
            commands: Notify::new(),
            stop: CancellationToken::new(),
            admissions: Arc::new(Semaphore::new(
                (database_connections / 2).clamp(1, 16) as usize
            )),
            draining: AtomicBool::new(false),
            telemetry_client: tokio::sync::OnceCell::new(),
        }
    }

    /// Stable identity of this process, stored with each accepted device session.
    pub fn id(&self) -> Uuid {
        self.id
    }
    /// Whether this process has stopped accepting new device sessions and assignments.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Begin readiness drain while keeping existing leases alive during propagation.
    pub fn drain(&self) {
        self.draining.store(true, Ordering::Release);
        self.wake.notify_waiters();
        self.commands.notify_waiters();
    }

    /// Terminate active streams after readiness propagation has completed.
    pub fn stop(&self) {
        self.stop.cancel();
    }

    /// Wait until this replica has completed its readiness drain.
    pub async fn stopped(&self) {
        self.stop.cancelled().await;
    }
}
