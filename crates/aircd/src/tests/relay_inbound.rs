use super::*;

// ---------------------------------------------------------------------------
// Tests — subscribe side (inbound: relay → local state + notifications)
// ---------------------------------------------------------------------------

/// Helper: build a `SharedState` and start the relay event loop (the
/// select loop from server.rs that calls `handle_relay_event`). Returns
/// the state and the relay for injection.
pub(super) async fn setup_with_relay_loop() -> (SharedState, Arc<TestRelay>) {
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
async fn inbound_client_intro_registers_remote_client() {
    let (state, relay) = setup_with_relay_loop().await;

    // Allocate a stable ClientId from outside the local counter range.
    let remote_id = ClientId(9001);
    let remote_client = make_remote_client(
        remote_id,
        "remote_alice",
        "user",
        "remote.host",
        NodeId("node-2".to_string()),
    );

    // Inject a ClientIntro event as if it came from a remote node.
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_client.node_id().unwrap().clone(),
            client: remote_client,
        })
        .await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // The remote client should now be in the registry.
    let found = state.find_user_by_nick("remote_alice").await;
    assert!(
        found.is_some(),
        "expected remote_alice to be in the registry"
    );
    let found = found.unwrap();
    assert!(found.is_remote(), "expected client to be Remote, got local");
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

    // Register remote client first via ClientIntro.
    let remote_id = ClientId(9002);
    let remote_client = make_remote_client(
        remote_id,
        "remote_bob",
        "user",
        "remote.host",
        NodeId("node-2".to_string()),
    );
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_client.node_id().unwrap().clone(),
            client: remote_client,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Remote user joins #shared.
    relay
        .inject(RelayEvent::Join {
            client_id: remote_id,
            channel: "#shared".to_string(),
        })
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
            .starts_with("remote_bob!"),
        "prefix should start with 'remote_bob!', got: {:?}",
        join_received.prefix
    );

    // Channel should now have the remote member.
    assert!(
        state.is_channel_member("#shared", "remote_bob").await,
        "remote_bob should be a member of #shared"
    );
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

    // Register remote client and join them to the channel.
    let remote_id = ClientId(9003);
    let remote_client = make_remote_client(
        remote_id,
        "speaker",
        "user",
        "remote.host",
        NodeId("node-3".to_string()),
    );
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_client.node_id().unwrap().clone(),
            client: remote_client,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    relay
        .inject(RelayEvent::Join {
            client_id: remote_id,
            channel: "#relay".to_string(),
        })
        .await;
    local
        .recv_until(Command::Join, Duration::from_secs(2))
        .await;
    local.drain().await;

    // Remote user sends a PRIVMSG to the channel.
    relay
        .inject(RelayEvent::Privmsg {
            client_id: remote_id,
            target: "#relay".to_string(),
            text: "hello from remote".to_string(),
        })
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

    // Register the remote sender first.
    let remote_id = ClientId(9004);
    let remote_sender = make_remote_client(
        remote_id,
        "remote_sender",
        "user",
        "remote.host",
        NodeId("node-4".to_string()),
    );
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_sender.node_id().unwrap().clone(),
            client: remote_sender,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Remote node sends a DM to our local user.
    relay
        .inject(RelayEvent::Privmsg {
            client_id: remote_id,
            target: "dm_target".to_string(),
            text: "private hello".to_string(),
        })
        .await;

    let msgs = local
        .recv_until(Command::Privmsg, Duration::from_secs(2))
        .await;
    let received = msgs.iter().find(|m| m.command == Command::Privmsg).unwrap();
    assert_eq!(received.params[0], "dm_target");
    assert_eq!(received.params[1], "private hello");
}

#[tokio::test]
async fn inbound_quit_removes_remote_client_and_notifies_local() {
    let (state, relay) = setup_with_relay_loop().await;

    // Connect a local client and join #room.
    let mut local = connect_client(&state, "watcher").await;
    local.send("JOIN #room").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Register remote client and join them to #room.
    let remote_id = ClientId(9005);
    let remote_client = make_remote_client(
        remote_id,
        "leaver",
        "user",
        "remote.host",
        NodeId("node-5".to_string()),
    );
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_client.node_id().unwrap().clone(),
            client: remote_client,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    relay
        .inject(RelayEvent::Join {
            client_id: remote_id,
            channel: "#room".to_string(),
        })
        .await;
    local
        .recv_until(Command::Join, Duration::from_secs(2))
        .await;
    local.drain().await;

    // Verify the remote client is registered.
    assert!(
        state.find_user_by_nick("leaver").await.is_some(),
        "leaver should be registered"
    );

    // Remote user quits.
    relay
        .inject(RelayEvent::Quit {
            client_id: remote_id,
            reason: Some("gone".to_string()),
        })
        .await;

    let msgs = local
        .recv_until(Command::Quit, Duration::from_secs(2))
        .await;
    let quit = msgs.iter().find(|m| m.command == Command::Quit).unwrap();
    assert_eq!(quit.params[0], "gone");

    // Give state cleanup a moment.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Client should be gone.
    assert!(
        state.find_user_by_nick("leaver").await.is_none(),
        "leaver should have been removed"
    );

    // Channel should no longer have the remote member.
    assert!(
        !state.is_channel_member("#room", "leaver").await,
        "leaver should not be a member of #room after quit"
    );
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

    // Register remote client and join them.
    let remote_id = ClientId(9006);
    let remote_client = make_remote_client(
        remote_id,
        "parter",
        "user",
        "remote.host",
        NodeId("node-6".to_string()),
    );
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_client.node_id().unwrap().clone(),
            client: remote_client,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    relay
        .inject(RelayEvent::Join {
            client_id: remote_id,
            channel: "#parttest".to_string(),
        })
        .await;
    local
        .recv_until(Command::Join, Duration::from_secs(2))
        .await;
    local.drain().await;

    // Remote user parts #parttest.
    relay
        .inject(RelayEvent::Part {
            client_id: remote_id,
            channel: "#parttest".to_string(),
            reason: Some("later".to_string()),
        })
        .await;

    let msgs = local
        .recv_until(Command::Part, Duration::from_secs(2))
        .await;
    let part = msgs.iter().find(|m| m.command == Command::Part).unwrap();
    assert_eq!(part.params[0], "#parttest");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Channel should no longer have the remote member.
    assert!(
        !state.is_channel_member("#parttest", "parter").await,
        "parter should not be a member of #parttest after part"
    );
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

    // Register the remote client who will change the topic.
    let remote_id = ClientId(9007);
    let remote_op = make_remote_client(
        remote_id,
        "remote_op",
        "user",
        "remote.host",
        NodeId("node-7".to_string()),
    );
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_op.node_id().unwrap().clone(),
            client: remote_op,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Remote node changes the topic.
    relay
        .inject(RelayEvent::Topic {
            client_id: remote_id,
            channel: "#topicroom".to_string(),
            text: "New remote topic".to_string(),
        })
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

    // Register the remote client who will change the mode.
    let remote_id = ClientId(9008);
    let remote_op = make_remote_client(
        remote_id,
        "remote_op_mode",
        "user",
        "remote.host",
        NodeId("node-8".to_string()),
    );
    relay
        .inject(RelayEvent::ClientIntro {
            node_id: remote_op.node_id().unwrap().clone(),
            client: remote_op,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Remote node sets +i on the channel.
    relay
        .inject(RelayEvent::Mode {
            client_id: remote_id,
            target: "#modetest".to_string(),
            mode_string: "+i".to_string(),
        })
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
async fn node_down_removes_all_remote_clients_and_notifies() {
    let (state, relay) = setup_with_relay_loop().await;

    let node_id = NodeId("failing-node".to_string());

    // Connect local client, join channel.
    let mut local = connect_client(&state, "survivor").await;
    local.send("JOIN #netsplit").await;
    local
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    local.drain().await;

    // Add two remote clients from the same node, join them to #netsplit.
    let ghost1_id = ClientId(9010);
    let ghost2_id = ClientId(9011);

    for (id, nick) in [(ghost1_id, "ghost1"), (ghost2_id, "ghost2")] {
        let client = make_remote_client(id, nick, "user", "remote.host", node_id.clone());
        relay
            .inject(RelayEvent::ClientIntro {
                node_id: client.node_id().unwrap().clone(),
                client,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        relay
            .inject(RelayEvent::Join {
                client_id: id,
                channel: "#netsplit".to_string(),
            })
            .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Drain the JOIN notifications received by the local client.
    local.drain().await;

    // Verify they're present.
    assert!(
        state.find_user_by_nick("ghost1").await.is_some(),
        "ghost1 should be registered"
    );
    assert!(
        state.find_user_by_nick("ghost2").await.is_some(),
        "ghost2 should be registered"
    );

    // Simulate NodeDown.
    relay
        .inject(RelayEvent::NodeDown {
            node_id: node_id.clone(),
        })
        .await;

    // Local user should receive netsplit QUITs.
    let msgs = local
        .recv_until(Command::Quit, Duration::from_secs(2))
        .await;
    assert!(
        msgs.iter().any(|m| m.command == Command::Quit),
        "expected QUIT from netsplit"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Both remote clients should be gone.
    assert!(
        state.find_user_by_nick("ghost1").await.is_none(),
        "ghost1 should have been removed"
    );
    assert!(
        state.find_user_by_nick("ghost2").await.is_none(),
        "ghost2 should have been removed"
    );
}
