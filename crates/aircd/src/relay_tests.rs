//! Integration tests for the relay layer.
//!
//! Uses a `TestRelay` that captures published messages and allows injecting
//! inbound events, wired to real `SharedState` + `Connection` instances to
//! exercise the full publish/subscribe path without any network I/O.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;

use airc_shared::{Command, IrcMessage};

use crate::client::{ClientKind, NodeId};
use crate::config::ServerConfig;
use crate::connection::Connection;
use crate::relay::{BoxFuture, InboundEvent, Relay, RelayError, RelayedMessage};
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// TestRelay — captures published messages, injects inbound events
// ---------------------------------------------------------------------------

/// A relay backend for testing that records every published message and
/// exposes a sender to inject inbound events into the subscribe stream.
struct TestRelay {
    node_id: NodeId,
    /// Every message passed to `publish()` is appended here.
    published: Mutex<Vec<IrcMessage>>,
    /// Sender half — tests call `inject()` to push events into the
    /// subscriber's receiver.
    inbound_tx: mpsc::Sender<InboundEvent>,
    /// Receiver half — handed out by `subscribe()` (only once).
    inbound_rx: Mutex<Option<mpsc::Receiver<InboundEvent>>>,
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
    async fn inject(&self, event: InboundEvent) {
        self.inbound_tx.send(event).await.expect("inject failed");
    }

    /// Snapshot of all published messages so far.
    async fn published(&self) -> Vec<IrcMessage> {
        self.published.lock().await.clone()
    }

    /// Wait until at least `n` messages have been published (with timeout).
    async fn wait_published(&self, n: usize) -> Vec<IrcMessage> {
        for _ in 0..100 {
            let msgs = self.published.lock().await.clone();
            if msgs.len() >= n {
                return msgs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {} published messages, got {}",
            n,
            self.published.lock().await.len()
        );
    }
}

impl Relay for TestRelay {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn publish(&self, message: &IrcMessage) -> BoxFuture<'_, Result<(), RelayError>> {
        let msg = message.clone();
        Box::pin(async move {
            self.published.lock().await.push(msg);
            Ok(())
        })
    }

    fn subscribe(&self) -> BoxFuture<'_, Result<mpsc::Receiver<InboundEvent>, RelayError>> {
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
        conn.run_generic(BufReader::new(pipe_server), tx).await;
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

// ---------------------------------------------------------------------------
// Tests — publish side (outbound: local commands → relay)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registration_publishes_nick_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let _client = connect_client(&state, "alice").await;

    // Registration should have published a NICK message.
    let msgs = relay.wait_published(1).await;
    assert_eq!(msgs[0].command, Command::Nick);
    assert_eq!(msgs[0].params[0], "alice");
    // Prefix should be the client's nick!user@host.
    assert!(
        msgs[0].prefix.as_ref().unwrap().starts_with("alice!"),
        "prefix should start with 'alice!', got: {:?}",
        msgs[0].prefix
    );
}

#[tokio::test]
async fn join_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "bob").await;
    // Clear the NICK from registration.
    let _ = relay.wait_published(1).await;

    client.send("JOIN #test").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await; // RPL_ENDOFNAMES

    let msgs = relay.wait_published(2).await;
    let join_msg = &msgs[1]; // [0] is NICK from registration
    assert_eq!(join_msg.command, Command::Join);
    assert_eq!(join_msg.params[0], "#test");
}

#[tokio::test]
async fn part_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "carol").await;
    client.send("JOIN #room").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    // Wait for NICK + JOIN to be published.
    let _ = relay.wait_published(2).await;

    client.send("PART #room :bye").await;
    // Wait for the PART response.
    client
        .recv_until(Command::Part, Duration::from_secs(2))
        .await;

    let msgs = relay.wait_published(3).await;
    let part_msg = &msgs[2]; // [0]=NICK, [1]=JOIN, [2]=PART
    assert_eq!(part_msg.command, Command::Part);
    assert_eq!(part_msg.params[0], "#room");
    assert_eq!(part_msg.params.get(1).map(|s| s.as_str()), Some("bye"));
}

#[tokio::test]
async fn quit_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "dave").await;
    let _ = relay.wait_published(1).await;

    client.send("QUIT :see ya").await;
    // Give the handler time to process.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let msgs = relay.wait_published(2).await;
    let quit_msg = &msgs[1];
    assert_eq!(quit_msg.command, Command::Quit);
    assert_eq!(quit_msg.params[0], "see ya");
}

#[tokio::test]
async fn channel_privmsg_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "eve").await;
    client.send("JOIN #chat").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    let _ = relay.wait_published(2).await; // NICK + JOIN

    client.send("PRIVMSG #chat :hello world").await;
    // Small delay for async processing.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let msgs = relay.wait_published(3).await;
    let pm = &msgs[2];
    assert_eq!(pm.command, Command::Privmsg);
    assert_eq!(pm.params[0], "#chat");
    assert_eq!(pm.params[1], "hello world");
}

#[tokio::test]
async fn dm_to_remote_nick_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    // Register a remote nick so the DM routing hits the Remote branch.
    state
        .add_remote_nick("remote_user", NodeId("remote-node-1".to_string()))
        .await;

    let mut client = connect_client(&state, "frank").await;
    let _ = relay.wait_published(1).await; // NICK from registration

    client.send("PRIVMSG remote_user :hey there").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let msgs = relay.wait_published(2).await;
    let dm = &msgs[1];
    assert_eq!(dm.command, Command::Privmsg);
    assert_eq!(dm.params[0], "remote_user");
    assert_eq!(dm.params[1], "hey there");
}

#[tokio::test]
async fn nick_change_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "greg").await;
    let _ = relay.wait_published(1).await; // NICK from registration

    client.send("NICK newgreg").await;
    client
        .recv_until(Command::Nick, Duration::from_secs(2))
        .await;

    let msgs = relay.wait_published(2).await;
    let nick_msg = &msgs[1];
    assert_eq!(nick_msg.command, Command::Nick);
    assert_eq!(nick_msg.params[0], "newgreg");
    // Prefix should carry the old identity.
    assert!(
        nick_msg.prefix.as_ref().unwrap().starts_with("greg!"),
        "prefix should carry old nick, got: {:?}",
        nick_msg.prefix
    );
}

#[tokio::test]
async fn topic_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "hank").await;
    client.send("JOIN #topictest").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    let _ = relay.wait_published(2).await; // NICK + JOIN

    client.send("TOPIC #topictest :new topic").await;
    client
        .recv_until(Command::Topic, Duration::from_secs(2))
        .await;

    let msgs = relay.wait_published(3).await;
    let topic_msg = &msgs[2];
    assert_eq!(topic_msg.command, Command::Topic);
    assert_eq!(topic_msg.params[0], "#topictest");
    assert_eq!(topic_msg.params[1], "new topic");
}

#[tokio::test]
async fn kick_publishes_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut kicker = connect_client(&state, "op_user").await;
    let mut target = connect_client(&state, "kicked_user").await;

    kicker.send("JOIN #kicktest").await;
    kicker
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    target.send("JOIN #kicktest").await;
    target
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    // op_user is operator (first to join). Wait for publishes so far.
    // NICK(op_user) + NICK(kicked_user) + JOIN(op_user) + JOIN(kicked_user) = 4
    let _ = relay.wait_published(4).await;

    kicker.send("KICK #kicktest kicked_user :behave").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let msgs = relay.wait_published(5).await;
    let kick_msg = &msgs[4];
    assert_eq!(kick_msg.command, Command::Kick);
    assert_eq!(kick_msg.params[0], "#kicktest");
    assert_eq!(kick_msg.params[1], "kicked_user");
    assert_eq!(kick_msg.params[2], "behave");
}

#[tokio::test]
async fn unexpected_disconnect_publishes_quit_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let client = connect_client(&state, "vanisher").await;
    let _ = relay.wait_published(1).await; // NICK from registration

    // Disconnect without sending QUIT.
    client.disconnect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let msgs = relay.wait_published(2).await;
    let quit_msg = &msgs[1];
    assert_eq!(quit_msg.command, Command::Quit);
}

// ---------------------------------------------------------------------------
// Tests — subscribe side (inbound: relay → local state + notifications)
// ---------------------------------------------------------------------------

/// Helper: build a `SharedState` and start the relay event loop (the
/// select loop from server.rs that calls `handle_relay_event`). Returns
/// the state and the relay for injection.
async fn setup_with_relay_loop() -> (SharedState, Arc<TestRelay>) {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    // Start the inbound relay event loop in a background task.
    let relay_state = state.clone();
    let mut relay_rx = state.relay_subscribe().await.unwrap();
    tokio::spawn(async move {
        // Minimal replica of the server's select loop — just relay events.
        let server = crate::server::Server::new_for_test(relay_state);
        while let Some(event) = relay_rx.recv().await {
            server.handle_relay_event_for_test(event).await;
        }
    });

    (state, relay)
}

#[tokio::test]
async fn inbound_nick_registers_remote_nick() {
    let (state, relay) = setup_with_relay_loop().await;

    // Inject a NICK message from a remote node.
    let nick_msg = IrcMessage::nick("remote_alice").with_prefix("remote_alice!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-2".to_string()),
            message: nick_msg,
        }))
        .await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // The remote nick should now be in the registry.
    let kind = state.nick_kind("remote_alice").await;
    assert!(
        matches!(kind, Some(ClientKind::Remote(_))),
        "expected Remote, got: {kind:?}"
    );
}

#[tokio::test]
async fn inbound_join_adds_remote_channel_member_and_notifies_local() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect a local client and join a channel.
    let mut local = connect_client(&state, "local_user").await;
    local.send("JOIN #shared").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Register remote nick first.
    let nick_msg = IrcMessage::nick("remote_bob").with_prefix("remote_bob!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-2".to_string()),
            message: nick_msg,
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Remote user joins #shared.
    let join_msg = IrcMessage::join("#shared").with_prefix("remote_bob!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-2".to_string()),
            message: join_msg,
        }))
        .await;

    // The local client should see the JOIN.
    let msgs = local
        .recv_until(Command::Join, Duration::from_secs(2))
        .await;
    let join_received = msgs.iter().find(|m| m.command == Command::Join).unwrap();
    assert_eq!(join_received.params[0], "#shared");
    assert!(
        join_received
            .prefix
            .as_ref()
            .unwrap()
            .starts_with("remote_bob!")
    );

    // Channel should now have the remote member.
    let channel = state.get_channel("#shared").await.unwrap();
    assert!(channel.is_member_nick("remote_bob"));
}

#[tokio::test]
async fn inbound_privmsg_delivered_to_local_channel_members() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect a local client and join #relay.
    let mut local = connect_client(&state, "listener").await;
    local.send("JOIN #relay").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Add remote nick and join them to the channel via relay.
    let nick_msg = IrcMessage::nick("speaker").with_prefix("speaker!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-3".to_string()),
            message: nick_msg,
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let join_msg = IrcMessage::join("#relay").with_prefix("speaker!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-3".to_string()),
            message: join_msg,
        }))
        .await;
    local
        .recv_until(Command::Join, Duration::from_secs(2))
        .await;
    local.drain().await;

    // Remote user sends a PRIVMSG to the channel.
    let pm =
        IrcMessage::privmsg("#relay", "hello from remote").with_prefix("speaker!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-3".to_string()),
            message: pm,
        }))
        .await;

    let msgs = local
        .recv_until(Command::Privmsg, Duration::from_secs(2))
        .await;
    let received = msgs.iter().find(|m| m.command == Command::Privmsg).unwrap();
    assert_eq!(received.params[0], "#relay");
    assert_eq!(received.params[1], "hello from remote");
}

#[tokio::test]
async fn inbound_dm_delivered_to_local_nick() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect a local client.
    let mut local = connect_client(&state, "dm_target").await;
    local.drain().await;

    // Remote node sends a DM to our local user.
    let dm = IrcMessage::privmsg("dm_target", "private hello")
        .with_prefix("remote_sender!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-4".to_string()),
            message: dm,
        }))
        .await;

    let msgs = local
        .recv_until(Command::Privmsg, Duration::from_secs(2))
        .await;
    let received = msgs.iter().find(|m| m.command == Command::Privmsg).unwrap();
    assert_eq!(received.params[0], "dm_target");
    assert_eq!(received.params[1], "private hello");
}

#[tokio::test]
async fn inbound_quit_removes_remote_nick_and_notifies_local() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect a local client and join #room.
    let mut local = connect_client(&state, "watcher").await;
    local.send("JOIN #room").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Add a remote nick and join them to #room.
    let nick_msg = IrcMessage::nick("leaver").with_prefix("leaver!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-5".to_string()),
            message: nick_msg,
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let join_msg = IrcMessage::join("#room").with_prefix("leaver!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-5".to_string()),
            message: join_msg,
        }))
        .await;
    local
        .recv_until(Command::Join, Duration::from_secs(2))
        .await;
    local.drain().await;

    // Verify the remote nick is registered.
    assert!(state.nick_kind("leaver").await.is_some());

    // Remote user quits.
    let quit_msg = IrcMessage::quit(Some("gone")).with_prefix("leaver!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-5".to_string()),
            message: quit_msg,
        }))
        .await;

    let msgs = local
        .recv_until(Command::Quit, Duration::from_secs(2))
        .await;
    let quit = msgs.iter().find(|m| m.command == Command::Quit).unwrap();
    assert_eq!(quit.params[0], "gone");

    // Give state cleanup a moment.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Nick should be gone.
    assert!(state.nick_kind("leaver").await.is_none());

    // Channel should no longer have the remote member.
    if let Some(channel) = state.get_channel("#room").await {
        assert!(!channel.is_member_nick("leaver"));
    }
}

#[tokio::test]
async fn inbound_part_removes_remote_channel_member_and_notifies_local() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect a local client and join #parttest.
    let mut local = connect_client(&state, "stayer").await;
    local.send("JOIN #parttest").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Register remote nick and join them.
    let nick_msg = IrcMessage::nick("parter").with_prefix("parter!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-6".to_string()),
            message: nick_msg,
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let join_msg = IrcMessage::join("#parttest").with_prefix("parter!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-6".to_string()),
            message: join_msg,
        }))
        .await;
    local
        .recv_until(Command::Join, Duration::from_secs(2))
        .await;
    local.drain().await;

    // Remote user parts #parttest.
    let part_msg =
        IrcMessage::part("#parttest", Some("later")).with_prefix("parter!user@remote.host");
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-6".to_string()),
            message: part_msg,
        }))
        .await;

    let msgs = local
        .recv_until(Command::Part, Duration::from_secs(2))
        .await;
    let part = msgs.iter().find(|m| m.command == Command::Part).unwrap();
    assert_eq!(part.params[0], "#parttest");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Channel should no longer have the remote member.
    if let Some(channel) = state.get_channel("#parttest").await {
        assert!(!channel.is_member_nick("parter"));
    }
}

#[tokio::test]
async fn inbound_topic_updates_channel_and_notifies_local() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect local client, join channel.
    let mut local = connect_client(&state, "topicwatcher").await;
    local.send("JOIN #topicroom").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Remote node changes the topic.
    let topic_msg = IrcMessage {
        prefix: Some("remote_op!user@remote.host".to_string()),
        command: Command::Topic,
        params: vec!["#topicroom".to_string(), "New remote topic".to_string()],
    };
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-7".to_string()),
            message: topic_msg,
        }))
        .await;

    let msgs = local
        .recv_until(Command::Topic, Duration::from_secs(2))
        .await;
    let topic = msgs.iter().find(|m| m.command == Command::Topic).unwrap();
    assert_eq!(topic.params[0], "#topicroom");
    assert_eq!(topic.params[1], "New remote topic");

    // Verify state was updated.
    let channel = state.get_channel("#topicroom").await.unwrap();
    let (text, setter, _ts) = channel.topic.unwrap();
    assert_eq!(text, "New remote topic");
    assert_eq!(setter, "remote_op");
}

#[tokio::test]
async fn inbound_mode_updates_channel_state() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect local client, join channel.
    let mut local = connect_client(&state, "modeuser").await;
    local.send("JOIN #modetest").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Remote node sets +i on the channel.
    let mode_msg = IrcMessage {
        prefix: Some("remote_op!user@remote.host".to_string()),
        command: Command::Mode,
        params: vec!["#modetest".to_string(), "+i".to_string()],
    };
    relay
        .inject(InboundEvent::Message(RelayedMessage {
            source_node: NodeId("node-8".to_string()),
            message: mode_msg,
        }))
        .await;

    let msgs = local
        .recv_until(Command::Mode, Duration::from_secs(2))
        .await;
    let mode = msgs.iter().find(|m| m.command == Command::Mode).unwrap();
    assert_eq!(mode.params[0], "#modetest");
    assert_eq!(mode.params[1], "+i");

    // Verify state was updated.
    let channel = state.get_channel("#modetest").await.unwrap();
    assert!(channel.modes.invite_only, "channel should be +i");
}

#[tokio::test]
async fn node_down_removes_all_remote_nicks_and_notifies() {
    let (state, relay) = setup_with_relay_loop().await;

    let node_id = NodeId("failing-node".to_string());

    // Connect local client, join channel.
    let mut local = connect_client(&state, "survivor").await;
    local.send("JOIN #netsplit").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Add two remote nicks from the same node, join them to #netsplit.
    for nick in &["ghost1", "ghost2"] {
        state.add_remote_nick(nick, node_id.clone()).await;
        state
            .add_remote_channel_member("#netsplit", nick, node_id.clone())
            .await;
    }

    // Verify they're present.
    assert!(state.nick_kind("ghost1").await.is_some());
    assert!(state.nick_kind("ghost2").await.is_some());

    // Simulate NodeDown.
    relay
        .inject(InboundEvent::NodeDown {
            node_id: node_id.clone(),
        })
        .await;

    // Local user should receive a netsplit QUIT.
    let msgs = local
        .recv_until(Command::Quit, Duration::from_secs(2))
        .await;
    assert!(
        msgs.iter().any(|m| m.command == Command::Quit),
        "expected QUIT from netsplit"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Both remote nicks should be gone.
    assert!(state.nick_kind("ghost1").await.is_none());
    assert!(state.nick_kind("ghost2").await.is_none());
}
