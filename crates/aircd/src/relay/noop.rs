//! No-op relay — used in single-instance mode.
//!
//! Every method is a no-op that returns `Ok(())` immediately.  The
//! `subscribe()` channel never yields events.  This ensures **zero
//! overhead** when running a single aircd instance.

use rand::Rng;
use tokio::sync::mpsc;

use crate::client::NodeId;
use crate::relay::{BoxFuture, InboundEvent, Relay, RelayError};

use airc_shared::IrcMessage;

/// A relay that does nothing — for single-instance deployments.
#[allow(dead_code)] // node_id is accessed via the Relay trait.
pub struct NoopRelay {
    node_id: NodeId,
}

impl NoopRelay {
    /// Create a new no-op relay with a random node ID.
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        let id: String = (0..16)
            .map(|_| format!("{:02x}", rng.r#gen::<u8>()))
            .collect();
        Self {
            node_id: NodeId(id),
        }
    }
}

impl Relay for NoopRelay {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn publish(&self, _message: &IrcMessage) -> BoxFuture<'_, Result<(), RelayError>> {
        // Single instance — nothing to relay.
        Box::pin(async { Ok(()) })
    }

    fn subscribe(&self) -> BoxFuture<'_, Result<mpsc::Receiver<InboundEvent>, RelayError>> {
        Box::pin(async {
            // Return a channel that never yields — the sender is dropped immediately.
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        })
    }
}
