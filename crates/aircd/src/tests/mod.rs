//! Integration tests for the relay layer.
//!
//! Uses a `TestRelay` that captures published events and allows injecting
//! inbound events, wired to real `SharedState` + `Connection` instances to
//! exercise the full publish/subscribe path without any network I/O.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use airc_shared::{Command, IrcMessage};

use crate::client::{Client, ClientId, ClientInfo, NodeId};
use crate::config::ServerConfig;
use crate::connection::Connection;
use crate::persist::PersistentState;
use crate::relay::{BoxFuture, Relay, RelayError, RelayEvent};
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// TestRelay — captures published events, injects inbound events
// ---------------------------------------------------------------------------

/// A relay backend for testing that records every published event and
/// exposes a sender to inject inbound events into the subscribe stream.
struct TestRelay {
    node_id: NodeId,
    /// Every event passed to `publish()` is appended here.
    published: Mutex<Vec<RelayEvent>>,
    /// Sender half — tests call `inject()` to push events into the
    /// subscriber's receiver.
    inbound_tx: mpsc::Sender<RelayEvent>,
    /// Receiver half — handed out by `subscribe()` (only once).
    inbound_rx: Mutex<Option<mpsc::Receiver<RelayEvent>>>,
}

impl TestRelay {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel(64);
        Self {
            node_id: NodeId("test-node-local".to_string()),
            published: Mutex::new(Vec::new()),
            inbound_tx: tx,
            inbound_rx: Mutex::new(Some(rx)),
        }
    }

    /// Inject an inbound event as if it came from a remote node.
    async fn inject(&self, event: RelayEvent) {
        self.inbound_tx.send(event).await.expect("inject failed");
    }

    /// Wait until at least `n` events have been published (with timeout).
    /// Returns the count at that point.
    async fn wait_published_count(&self, n: usize) -> usize {
        for _ in 0..100 {
            let count = self.published.lock().await.len();
            if count >= n {
                return count;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let count = self.published.lock().await.len();
        panic!("timed out waiting for {n} published events, got {count}");
    }

    /// Apply a predicate to each published event in order, returning the
    /// index of the first match or panicking after a timeout.
    async fn wait_for_event<F>(&self, predicate: F) -> usize
    where
        F: Fn(&RelayEvent) -> bool,
    {
        for _ in 0..100 {
            let events = self.published.lock().await;
            for (i, ev) in events.iter().enumerate() {
                if predicate(ev) {
                    return i;
                }
            }
            drop(events);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for matching published event");
    }

    /// Run a closure over all currently-published events.
    async fn with_events<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&[RelayEvent]) -> R,
    {
        let events = self.published.lock().await;
        f(&events)
    }
}

impl Relay for TestRelay {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn publish(&self, event: RelayEvent) -> BoxFuture<'_, Result<(), RelayError>> {
        Box::pin(async move {
            self.published.lock().await.push(event);
            Ok(())
        })
    }

    fn subscribe(&self) -> BoxFuture<'_, Result<mpsc::Receiver<RelayEvent>, RelayError>> {
        Box::pin(async {
            let rx = self
                .inbound_rx
                .lock()
                .await
                .take()
                .expect("subscribe() called more than once");
            Ok(rx)
        })
    }
}

// ---------------------------------------------------------------------------
// Test harness helpers
// ---------------------------------------------------------------------------

/// A test client connected to the server via an in-process pipe.
struct TestClient {
    /// Write IRC lines here (the server reads them).
    writer: tokio::io::DuplexStream,
    /// Receive server responses here.
    rx: mpsc::Receiver<Arc<str>>,
    /// Handle to the spawned connection task.
    _task: tokio::task::JoinHandle<()>,
}

impl TestClient {
    /// Send a raw IRC line to the server (appends `\r\n`).
    async fn send(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .unwrap();
    }

    /// Receive all lines available within `dur`, returning them as parsed
    /// `IrcMessage`s. Non-parseable lines are silently dropped.
    async fn recv_all(&mut self, dur: Duration) -> Vec<IrcMessage> {
        let mut msgs = Vec::new();
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            match timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                self.rx.recv(),
            )
            .await
            {
                Ok(Some(line)) => {
                    if let Ok(msg) = IrcMessage::parse(line.trim_end()) {
                        msgs.push(msg);
                    }
                }
                _ => break,
            }
        }
        msgs
    }

    /// Wait until we receive a line whose command matches `cmd`, up to `dur`.
    /// Returns all messages received up to and including the match.
    async fn recv_until(&mut self, cmd: Command, dur: Duration) -> Vec<IrcMessage> {
        let mut msgs = Vec::new();
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            match timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                self.rx.recv(),
            )
            .await
            {
                Ok(Some(line)) => {
                    if let Ok(msg) = IrcMessage::parse(line.trim_end()) {
                        let matched = msg.command == cmd;
                        msgs.push(msg);
                        if matched {
                            return msgs;
                        }
                    }
                }
                _ => break,
            }
        }
        msgs
    }

    /// Drain the receive buffer (throw away everything currently queued).
    async fn drain(&mut self) {
        while self.rx.try_recv().is_ok() {}
        // Small sleep to let any in-flight messages arrive.
        tokio::time::sleep(Duration::from_millis(50)).await;
        while self.rx.try_recv().is_ok() {}
    }

    /// Shut down by closing the write half (simulates disconnect).
    async fn disconnect(mut self) {
        let _ = self.writer.shutdown().await;
        // Give the connection task time to clean up.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Build a `SharedState` backed by the given `TestRelay`.
fn test_state(relay: Arc<TestRelay>) -> SharedState {
    let config = ServerConfig {
        server_name: "test.irc.local".to_string(),
        motd: vec!["Test server".to_string()],
        ..Default::default()
    };
    SharedState::new(config, relay)
}

/// Connect a new client through an in-process pipe, completing the
/// NICK+USER registration handshake. Returns the `TestClient` ready
/// for command dispatch.
async fn connect_client(state: &SharedState, nick: &str) -> TestClient {
    let id = state.next_client_id();
    let (pipe_server, pipe_client) = tokio::io::duplex(8192);
    let (tx, rx) = mpsc::channel::<Arc<str>>(512);

    let conn = Connection::new(id, state.clone(), "127.0.0.1".to_string());
    let task = tokio::spawn(async move {
        conn.run_generic(BufReader::new(pipe_server), tx, CancellationToken::new())
            .await;
    });

    let mut client = TestClient {
        writer: pipe_client,
        rx,
        _task: task,
    };

    // Perform registration handshake.
    client.send(&format!("NICK {nick}")).await;
    client
        .send(&format!("USER {nick} 0 * :Test User {nick}"))
        .await;

    // Wait for the welcome burst to complete (ends with RPL_ENDOFMOTD = 376).
    client
        .recv_until(Command::Numeric(376), Duration::from_secs(2))
        .await;

    client
}

/// Build a remote `Client` for use in subscribe-side tests.
///
/// This mirrors what the relay layer does when it receives a `ClientIntro`:
/// it calls `state.add_remote_client(client)` to register the remote user.
fn make_remote_client(
    id: ClientId,
    nick: &str,
    username: &str,
    hostname: &str,
    node_id: NodeId,
) -> Client {
    let info = Arc::new(ClientInfo {
        nick: nick.to_string(),
        username: username.to_string(),
        realname: nick.to_string(),
        hostname: hostname.to_string(),
        registered: true,
        identified: false,
        account: None,
        modes: 0,
        away: None,
    });
    Client::new_remote(id, info, node_id)
}

mod relay_outbound;
mod relay_inbound;
mod crdt;
mod snapshot;
