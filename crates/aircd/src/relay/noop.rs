//! No-op relay — used in single-instance mode.
//!
//! In single-instance mode there are no remote nodes to relay to, so every
//! network method is a no-op.  The [`FileLogger`] is still owned here and
//! receives structured logging calls so the single-node CSV event log is
//! preserved.

use std::path::PathBuf;

use rand::Rng;
use tokio::sync::mpsc;

use crate::client::NodeId;
use crate::relay::{BoxFuture, Relay, RelayError, RelayEvent};
use airc_shared::log::FileLogger;

/// A relay that does nothing — for single-instance deployments.
///
/// Owns a [`FileLogger`] that records every IRC event passing through the
/// typed publish methods.
pub struct NoopRelay {
    node_id: NodeId,
    #[allow(dead_code)]
    logger: FileLogger,
}

impl NoopRelay {
    /// Create a new no-op relay with a random node ID.
    ///
    /// Pass `Some(dir)` for `log_dir` to enable CSV event logging under that
    /// directory.  Pass `None` to disable logging (all writes become no-ops).
    pub fn new(log_dir: Option<PathBuf>) -> Self {
        let mut rng = rand::thread_rng();
        let id: String = (0..16)
            .map(|_| format!("{:02x}", rng.r#gen::<u8>()))
            .collect();
        let node_id = NodeId(id);
        let logger = FileLogger::new(log_dir, node_id.0.clone());
        Self { node_id, logger }
    }
}

impl Relay for NoopRelay {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn publish(&self, _event: RelayEvent) -> BoxFuture<'_, Result<(), RelayError>> {
        // Single instance — no remote nodes to notify.
        Box::pin(async { Ok(()) })
    }

    fn subscribe(&self) -> BoxFuture<'_, Result<mpsc::Receiver<RelayEvent>, RelayError>> {
        Box::pin(async {
            // Return a channel that never yields — the sender is dropped immediately.
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        })
    }
}
