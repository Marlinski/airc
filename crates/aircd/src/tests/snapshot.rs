use super::*;

// ---------------------------------------------------------------------------
// StateSnapshot tests
// ---------------------------------------------------------------------------

/// When a `NodeUp` event is received, the local node should publish a
/// `StateSnapshot` targeted at the new node containing all its local clients
/// and channel memberships.
#[tokio::test]
async fn node_up_triggers_state_snapshot_published() {
    // Use setup_with_relay_loop so handle_relay_event is actually called.
    let (state, relay) = super::relay_inbound::setup_with_relay_loop().await;

    // Connect a local client and join a channel so there is running state to snapshot.
    let mut alice = connect_client(&state, "alice").await;
    alice.send("JOIN #general").await;
    alice
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    // Wait for ClientIntro + Join to be published (so we know state is settled).
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::Join { .. }))
        .await;

    // Clear published events so we can check only what NodeUp triggers.
    relay.published.lock().await.clear();

    // A new remote node comes online.
    let new_node = NodeId("new-node-xyz".to_string());
    relay
        .inject(RelayEvent::NodeUp {
            node_id: new_node.clone(),
        })
        .await;

    // Wait for the StateSnapshot to be published.
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::StateSnapshot { .. }))
        .await;

    relay
        .with_events(|events| {
            let snap = events
                .iter()
                .find(|ev| matches!(ev, RelayEvent::StateSnapshot { .. }));
            assert!(
                snap.is_some(),
                "expected StateSnapshot to be published on NodeUp"
            );

            if let Some(RelayEvent::StateSnapshot {
                target_node_id,
                clients,
                channels,
                memberships,
            }) = snap
            {
                // Snapshot must be targeted at the new node.
                assert_eq!(
                    target_node_id.0, "new-node-xyz",
                    "snapshot should target the new node"
                );

                // Alice should be in the clients list.
                let has_alice = clients.iter().any(|c| c.info.nick == "alice");
                assert!(
                    has_alice,
                    "snapshot should include alice, got: {:?}",
                    clients.iter().map(|c| &c.info.nick).collect::<Vec<_>>()
                );

                // #general should be in the channels list.
                let has_general = channels.iter().any(|c| c.name == "#general");
                assert!(has_general, "snapshot should include #general");

                // Membership for alice in #general should be present.
                assert!(
                    !memberships.is_empty(),
                    "snapshot should include memberships"
                );
            }
        })
        .await;
}

/// Full two-node test: node A has a connected client in #general.  We build
/// a `StateSnapshot` from node A and inject it into an isolated node B.
/// After applying, node B should know about alice, the #general channel, and
/// alice's membership — without any prior ClientIntro or Join relay events.
#[tokio::test]
async fn node_up_state_snapshot_populates_remote_state() {
    // ---- Node A: isolated TestRelay (no peer) + relay loop ----------------
    let relay_a = Arc::new(TestRelay::new());
    let state_a = test_state(relay_a.clone());
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

    // Connect alice on node A and join #general.
    let mut alice = connect_client(&state_a, "alice").await;
    alice.send("JOIN #general").await;
    alice
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    // Wait for the Join relay event to be published (state settled on A).
    relay_a
        .wait_for_event(|ev| matches!(ev, RelayEvent::Join { .. }))
        .await;

    // ---- Node B: isolated TestRelay (no peer) + relay loop ----------------
    // Node B has no prior knowledge of alice or #general.
    let relay_b = Arc::new(TestRelay::new());
    let config_b = ServerConfig {
        server_name: "node-snap-b.local".to_string(),
        motd: vec!["node B".to_string()],
        ..Default::default()
    };
    let state_b = SharedState::new(config_b, relay_b.clone());
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

    // Sanity: node B knows nothing about alice yet.
    assert!(
        state_b.find_user_by_nick("alice").await.is_none(),
        "node B should not know about alice before snapshot"
    );
    assert!(
        state_b.get_channel("#general").await.is_none(),
        "node B should not have #general before snapshot"
    );

    // ---- Build snapshot from A and inject it into B -----------------------
    // target_node_id must match B's relay node_id so B applies (not ignores) it.
    let b_node_id = relay_b.node_id().clone();
    let snapshot = state_a.build_state_snapshot(b_node_id).await;

    // Inject the snapshot as an inbound event on node B.
    relay_b.inject(snapshot).await;

    // Give node B's spawned apply task time to complete.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Node B should now know about alice as a remote client.
    let found = state_b.find_user_by_nick("alice").await;
    assert!(
        found.is_some(),
        "node B should know about alice after StateSnapshot"
    );
    assert!(
        found.unwrap().is_remote(),
        "alice should be a remote client on node B"
    );

    // Node B should have #general in its channel map.
    assert!(
        state_b.get_channel("#general").await.is_some(),
        "node B should have #general after StateSnapshot"
    );

    // Alice should be a member of #general on node B.
    assert!(
        state_b.is_channel_member("#general", "alice").await,
        "alice should be a member of #general on node B"
    );
}

/// End-to-end cluster join test.
///
/// Node A runs with two clients (alice, bob) both in #lobby.  Node B then
/// joins the cluster by publishing `NodeUp`.  The full automatic path is
/// exercised:
///
///   NodeUp (B→A) → build_state_snapshot (on A) → StateSnapshot (A→B)
///   → apply_state_snapshot (on B)
///
/// After the round-trip node B should have alice and bob as remote clients
/// and #lobby with both memberships — without any manual snapshot injection.
///
/// # Why B's relay loop starts late
///
/// `PairRelay` forwards every published event to the peer's subscriber
/// immediately.  If B's relay loop were running during node A's warm-up,
/// it would receive the `ClientIntro` and `Join` events published when
/// alice and bob register/join, giving B prior knowledge before `NodeUp`.
/// Starting B's loop only after A's state is settled ensures B starts
/// empty and receives state exclusively via the `StateSnapshot`.
#[tokio::test]
async fn node_up_propagates_running_state_to_joining_node() {
    use super::crdt::PairRelay;

    let (relay_a, relay_b) = PairRelay::new_pair("snap-node-a", "snap-node-b");

    let config_a = crate::config::ServerConfig {
        server_name: "snap-node-a.local".to_string(),
        motd: vec!["node A".to_string()],
        ..Default::default()
    };
    let config_b = crate::config::ServerConfig {
        server_name: "snap-node-b.local".to_string(),
        motd: vec!["node B".to_string()],
        ..Default::default()
    };

    let state_a = SharedState::new(config_a, relay_a);
    let state_b = SharedState::new(config_b, relay_b);

    // Start node A's relay loop.  Node B's loop is intentionally not started
    // yet — see doc comment above.
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

    // Connect alice and bob on node A and join #lobby.  Any ClientIntro/Join
    // events published here go into PairRelay's channel for B, but since B has
    // no subscriber yet those sends are dropped (channel capacity exhausted or
    // simply unread — either way B never sees them).
    let mut alice = connect_client(&state_a, "alice").await;
    alice.send("JOIN #lobby").await;
    alice
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    let mut bob = connect_client(&state_a, "bob").await;
    bob.send("JOIN #lobby").await;
    bob.recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    // Give node A's relay loop a moment to process the Join events so state
    // is fully settled before node B comes online.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now start node B's relay loop.  From this point on B receives events.
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

    // Sanity: node B starts with no knowledge of alice, bob, or #lobby.
    assert!(
        state_b.find_user_by_nick("alice").await.is_none(),
        "node B should not know about alice before NodeUp"
    );
    assert!(
        state_b.find_user_by_nick("bob").await.is_none(),
        "node B should not know about bob before NodeUp"
    );
    assert!(
        state_b.get_channel("#lobby").await.is_none(),
        "node B should not have #lobby before NodeUp"
    );

    // Node B announces itself.  This NodeUp event is published via PairRelay
    // and lands in node A's relay loop, which calls build_state_snapshot and
    // publishes a StateSnapshot back.  That snapshot reaches node B via the
    // pair and is applied by B's relay loop.
    state_b
        .relay()
        .publish(RelayEvent::NodeUp {
            node_id: state_b.relay().node_id().clone(),
        })
        .await
        .unwrap();

    // Allow time for: NodeUp delivery → A builds snapshot → snapshot delivery
    // → B applies snapshot (two relay loop iterations + apply_state_snapshot).
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ── Assertions ───────────────────────────────────────────────────────────

    let found_alice = state_b.find_user_by_nick("alice").await;
    assert!(
        found_alice.is_some(),
        "node B should know about alice after NodeUp"
    );
    assert!(
        found_alice.unwrap().is_remote(),
        "alice should be a remote client on node B"
    );

    let found_bob = state_b.find_user_by_nick("bob").await;
    assert!(
        found_bob.is_some(),
        "node B should know about bob after NodeUp"
    );
    assert!(
        found_bob.unwrap().is_remote(),
        "bob should be a remote client on node B"
    );

    assert!(
        state_b.get_channel("#lobby").await.is_some(),
        "node B should have #lobby after NodeUp"
    );
    assert!(
        state_b.is_channel_member("#lobby", "alice").await,
        "alice should be a member of #lobby on node B"
    );
    assert!(
        state_b.is_channel_member("#lobby", "bob").await,
        "bob should be a member of #lobby on node B"
    );
}
