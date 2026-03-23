use super::*;

// ---------------------------------------------------------------------------
// PairRelay — bidirectional channel-backed relay for two-node tests
// ---------------------------------------------------------------------------

/// A relay backend that forms a bidirectional pair with another `PairRelay`.
///
/// Events published by one side are injected as `RelayEvent`s into the other
/// side's subscribe stream.  This exercises the full relay path without Redis.
///
/// Construct via `PairRelay::new_pair()`.
pub(super) struct PairRelay {
    node_id: NodeId,
    /// Sends events to the *other* relay's subscriber.
    peer_tx: mpsc::Sender<RelayEvent>,
    /// Receives events from the *other* relay.
    inbound_rx: Mutex<Option<mpsc::Receiver<RelayEvent>>>,
}

impl PairRelay {
    /// Create a matched pair of relays wired together.
    pub(super) fn new_pair(node_a: &str, node_b: &str) -> (Arc<Self>, Arc<Self>) {
        let (tx_a_to_b, rx_b) = mpsc::channel(256);
        let (tx_b_to_a, rx_a) = mpsc::channel(256);

        let relay_a = Arc::new(Self {
            node_id: NodeId(node_a.to_string()),
            peer_tx: tx_a_to_b,
            inbound_rx: Mutex::new(Some(rx_a)),
        });
        let relay_b = Arc::new(Self {
            node_id: NodeId(node_b.to_string()),
            peer_tx: tx_b_to_a,
            inbound_rx: Mutex::new(Some(rx_b)),
        });
        (relay_a, relay_b)
    }
}

impl Relay for PairRelay {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn publish(&self, event: RelayEvent) -> BoxFuture<'_, Result<(), RelayError>> {
        Box::pin(async move {
            let _ = self.peer_tx.send(event).await;
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
                .expect("PairRelay::subscribe() called more than once");
            Ok(rx)
        })
    }
}

// ---------------------------------------------------------------------------
// Two-node test harness
// ---------------------------------------------------------------------------

/// Build two `SharedState` instances connected via `PairRelay`, each with
/// an in-memory SQLite DB (`:memory:` is unique per connection).
/// Returns `(state_a, state_b)` with the relay event loops already running.
async fn setup_pair(
    node_a: &str,
    node_b: &str,
    db_path_a: &std::path::Path,
    db_path_b: &std::path::Path,
) -> (SharedState, SharedState) {
    let (relay_a, relay_b) = PairRelay::new_pair(node_a, node_b);

    let config_a = crate::config::ServerConfig {
        server_name: format!("{node_a}.local"),
        motd: vec!["node A".to_string()],
        ..Default::default()
    };
    let config_b = crate::config::ServerConfig {
        server_name: format!("{node_b}.local"),
        motd: vec!["node B".to_string()],
        ..Default::default()
    };

    let state_a = SharedState::new(config_a, relay_a);
    let state_b = SharedState::new(config_b, relay_b);

    // Open persistent state for each node.
    let ps_a = PersistentState::open(db_path_a, node_a)
        .await
        .expect("open ps_a");
    let ps_b = PersistentState::open(db_path_b, node_b)
        .await
        .expect("open ps_b");

    state_a.set_persistent(ps_a);
    state_b.set_persistent(ps_b);

    // Wire gossip channels (bounded, matching production config).
    {
        let ps = state_a.persistent().unwrap();
        let (tx, mut rx) = mpsc::channel::<(String, Vec<u8>)>(2048);
        ps.set_gossip_tx(tx);
        let relay_clone = state_a.clone();
        tokio::spawn(async move {
            while let Some((crdt_id, payload)) = rx.recv().await {
                let _ = relay_clone
                    .relay()
                    .publish(RelayEvent::CrdtDelta { crdt_id, payload })
                    .await;
            }
        });
    }
    {
        let ps = state_b.persistent().unwrap();
        let (tx, mut rx) = mpsc::channel::<(String, Vec<u8>)>(2048);
        ps.set_gossip_tx(tx);
        let relay_clone = state_b.clone();
        tokio::spawn(async move {
            while let Some((crdt_id, payload)) = rx.recv().await {
                let _ = relay_clone
                    .relay()
                    .publish(RelayEvent::CrdtDelta { crdt_id, payload })
                    .await;
            }
        });
    }

    // Start relay event loops for both nodes.
    {
        let s = state_a.clone();
        let mut rx = s.relay_subscribe().await.unwrap();
        tokio::spawn(async move {
            let server = crate::server::Server::new_for_test(s);
            while let Some(event) = rx.recv().await {
                server.handle_relay_event_for_test(event).await;
            }
        });
    }
    {
        let s = state_b.clone();
        let mut rx = s.relay_subscribe().await.unwrap();
        tokio::spawn(async move {
            let server = crate::server::Server::new_for_test(s);
            while let Some(event) = rx.recv().await {
                server.handle_relay_event_for_test(event).await;
            }
        });
    }

    (state_a, state_b)
}

// ---------------------------------------------------------------------------
// Phase C integration tests
// ---------------------------------------------------------------------------

/// Test 1: Ban added on node A gossips to node B.
///
/// Node A adds a ban on `#gchan`. The CRDT delta is gossiped via
/// `PairRelay` to node B. Node B should reflect the ban after merging.
#[tokio::test]
async fn crdt_ban_gossips_from_node_a_to_node_b() {
    let dir = tempfile::tempdir().unwrap();
    let db_a = dir.path().join("a.db");
    let db_b = dir.path().join("b.db");

    let (state_a, state_b) = setup_pair("node-a", "node-b", &db_a, &db_b).await;

    let ps_a = state_a.persistent().unwrap();
    let ps_b = state_b.persistent().unwrap();

    // Node A adds a ban.
    let added = ps_a
        .add_ban("#gchan", "*!*@badhost.example".to_string())
        .await;
    assert!(added, "ban should have been added on node A");

    // Give gossip time to propagate through PairRelay.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Node B should now know about the ban.
    let bans_b = ps_b.get_bans("#gchan").await;
    assert!(
        bans_b.iter().any(|b| b.contains("badhost.example")),
        "node B should have the ban after gossip, got: {bans_b:?}"
    );
}

/// Test 2: Nick registered on node A (pre-populated SQLite) propagates to
/// node B (empty SQLite) via anti-entropy on `NodeUp`.
///
/// Sequence:
///   1. Node A opens its DB, registers nick "alice".
///   2. Node B is fresh — no nick registrations.
///   3. Node B gathers its (empty) hashes and sends an anti-entropy request.
///   4. Node A sees the request and responds with its nick blob.
///   5. Node B merges → now has "alice".
#[tokio::test]
async fn anti_entropy_syncs_nick_from_populated_node_to_empty_node() {
    let dir = tempfile::tempdir().unwrap();
    let db_a = dir.path().join("a.db");
    let db_b = dir.path().join("b.db");

    let (state_a, state_b) = setup_pair("node-a", "node-b", &db_a, &db_b).await;

    let ps_a = state_a.persistent().unwrap();
    let ps_b = state_b.persistent().unwrap();

    // Pre-populate node A with a nick registration.
    ps_a.upsert_nick(crate::persist::NickRecord {
        nick: "alice".to_string(),
        scram_stored_key: Some("aabbcc".repeat(8).chars().take(64).collect()),
        scram_server_key: Some("ddeeff".repeat(8).chars().take(64).collect()),
        scram_salt: Some("deadbeef".repeat(4)),
        scram_iterations: Some(600_000),
        bcrypt_hash: Some("$2b$12$fakehashfortesting".to_string()),
        pubkey_hex: None,
        registered_at: 1_700_000_000,
        reputation: 0,
        capabilities: vec![],
    })
    .await;

    // Node B is empty — verify it has no registration.
    assert!(
        ps_b.get_nick("alice").await.is_none(),
        "node B starts empty"
    );

    // Step 1: node B gathers its (empty) hashes.
    let b_hashes = ps_b.all_crdt_hashes().await;
    let b_node_id = state_b.relay().node_id().to_string();

    // Step 2: node B publishes an anti-entropy request — this goes into
    // node A's inbound event queue via PairRelay.
    state_b
        .relay()
        .publish(RelayEvent::AntiEntropyRequest {
            from_node: b_node_id,
            hashes: b_hashes,
        })
        .await
        .unwrap();

    // Give node A time to process the request and respond.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Node B should now have alice's registration.
    let record = ps_b.get_nick("alice").await;
    assert!(
        record.is_some(),
        "node B should have alice after anti-entropy, got None"
    );
    assert_eq!(record.unwrap().nick, "alice");
}
