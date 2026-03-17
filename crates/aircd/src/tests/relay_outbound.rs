use super::*;

// ---------------------------------------------------------------------------
// Tests — publish side (outbound: local commands → relay typed events)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registration_publishes_client_intro_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let _client = connect_client(&state, "alice").await;

    // Registration should have published a ClientIntro event.
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::ClientIntro { .. }))
        .await;

    relay
        .with_events(|events| {
            let intro = events
                .iter()
                .find(|ev| matches!(ev, RelayEvent::ClientIntro { .. }));
            assert!(intro.is_some(), "expected ClientIntro event");
            if let Some(RelayEvent::ClientIntro { client, .. }) = intro {
                assert_eq!(client.info.nick, "alice");
            }
        })
        .await;
}

#[tokio::test]
async fn join_publishes_join_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "bob").await;
    // Wait for ClientIntro from registration.
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::ClientIntro { .. }))
        .await;

    client.send("JOIN #test").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await; // RPL_ENDOFNAMES

    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::Join { channel, .. } if channel == "#test"))
        .await;

    relay
        .with_events(|events| {
            let join = events
                .iter()
                .find(|ev| matches!(ev, RelayEvent::Join { channel, .. } if channel == "#test"));
            assert!(join.is_some(), "expected Join event for #test");
        })
        .await;
}

#[tokio::test]
async fn part_publishes_part_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "carol").await;
    client.send("JOIN #room").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;

    // Wait for ClientIntro + Join.
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::Join { .. }))
        .await;

    client.send("PART #room :bye").await;
    client
        .recv_until(Command::Part, Duration::from_secs(2))
        .await;

    relay
        .wait_for_event(|ev| {
            matches!(ev, RelayEvent::Part { channel, reason, .. }
            if channel == "#room" && reason.as_deref() == Some("bye"))
        })
        .await;

    relay
        .with_events(|events| {
            let part = events
                .iter()
                .find(|ev| matches!(ev, RelayEvent::Part { channel, .. } if channel == "#room"));
            assert!(part.is_some(), "expected Part event for #room");
            if let Some(RelayEvent::Part {
                channel, reason, ..
            }) = part
            {
                assert_eq!(channel, "#room");
                assert_eq!(reason.as_deref(), Some("bye"));
            }
        })
        .await;
}

#[tokio::test]
async fn quit_publishes_quit_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "dave").await;
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::ClientIntro { .. }))
        .await;

    client.send("QUIT :see ya").await;
    // Give the handler time to process.
    tokio::time::sleep(Duration::from_millis(100)).await;

    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::Quit { .. }))
        .await;

    relay.with_events(|events| {
        let quit = events.iter().find(|ev| matches!(ev, RelayEvent::Quit { reason, .. } if reason.as_deref() == Some("see ya")));
        assert!(quit.is_some(), "expected Quit event with reason 'see ya'");
    }).await;
}

#[tokio::test]
async fn channel_privmsg_publishes_privmsg_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "eve").await;
    client.send("JOIN #chat").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::Join { .. }))
        .await;

    client.send("PRIVMSG #chat :hello world").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    relay
        .wait_for_event(|ev| {
            matches!(ev, RelayEvent::Privmsg { target, text, .. }
            if target == "#chat" && text == "hello world")
        })
        .await;

    relay
        .with_events(|events| {
            let pm = events
                .iter()
                .find(|ev| matches!(ev, RelayEvent::Privmsg { target, .. } if target == "#chat"));
            assert!(pm.is_some(), "expected Privmsg event to #chat");
            if let Some(RelayEvent::Privmsg { target, text, .. }) = pm {
                assert_eq!(target, "#chat");
                assert_eq!(text, "hello world");
            }
        })
        .await;
}

#[tokio::test]
async fn dm_to_remote_nick_publishes_privmsg_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    // Register a remote user so the DM routing hits the Remote branch.
    let remote_id = ClientId(9001);
    let remote_client = make_remote_client(
        remote_id,
        "remote_user",
        "user",
        "remote.host",
        NodeId("remote-node-1".to_string()),
    );
    state.add_remote_client(remote_client).await;

    let mut client = connect_client(&state, "frank").await;
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::ClientIntro { .. }))
        .await;

    client.send("PRIVMSG remote_user :hey there").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    relay
        .wait_for_event(|ev| {
            matches!(ev, RelayEvent::Privmsg { target, text, .. }
            if target == "remote_user" && text == "hey there")
        })
        .await;

    relay
        .with_events(|events| {
            let dm = events.iter().find(
                |ev| matches!(ev, RelayEvent::Privmsg { target, .. } if target == "remote_user"),
            );
            assert!(dm.is_some(), "expected Privmsg event to remote_user");
            if let Some(RelayEvent::Privmsg { target, text, .. }) = dm {
                assert_eq!(target, "remote_user");
                assert_eq!(text, "hey there");
            }
        })
        .await;
}

#[tokio::test]
async fn nick_change_publishes_nick_change_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "greg").await;
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::ClientIntro { .. }))
        .await;

    client.send("NICK newgreg").await;
    client
        .recv_until(Command::Nick, Duration::from_secs(2))
        .await;

    relay
        .wait_for_event(
            |ev| matches!(ev, RelayEvent::NickChange { new_nick, .. } if new_nick == "newgreg"),
        )
        .await;

    relay
        .with_events(|events| {
            let nick_ev = events.iter().find(
                |ev| matches!(ev, RelayEvent::NickChange { new_nick, .. } if new_nick == "newgreg"),
            );
            assert!(
                nick_ev.is_some(),
                "expected NickChange event with new_nick='newgreg'"
            );
        })
        .await;
}

#[tokio::test]
async fn topic_publishes_topic_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let mut client = connect_client(&state, "hank").await;
    client.send("JOIN #topictest").await;
    client
        .recv_until(Command::Numeric(366), Duration::from_secs(2))
        .await;
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::Join { .. }))
        .await;

    client.send("TOPIC #topictest :new topic").await;
    client
        .recv_until(Command::Topic, Duration::from_secs(2))
        .await;

    relay
        .wait_for_event(|ev| {
            matches!(ev, RelayEvent::Topic { channel, text, .. }
            if channel == "#topictest" && text == "new topic")
        })
        .await;

    relay
        .with_events(|events| {
            let topic_ev = events.iter().find(
                |ev| matches!(ev, RelayEvent::Topic { channel, .. } if channel == "#topictest"),
            );
            assert!(topic_ev.is_some(), "expected Topic event for #topictest");
            if let Some(RelayEvent::Topic { channel, text, .. }) = topic_ev {
                assert_eq!(channel, "#topictest");
                assert_eq!(text, "new topic");
            }
        })
        .await;
}

#[tokio::test]
async fn kick_publishes_kick_event_to_relay() {
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

    // Wait for both ClientIntros and both Joins to be published.
    relay.wait_published_count(4).await; // ClientIntro(op) + ClientIntro(kicked) + Join(op) + Join(kicked)

    kicker.send("KICK #kicktest kicked_user :behave").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    relay
        .wait_for_event(|ev| {
            matches!(ev, RelayEvent::Kick { channel, reason, .. }
            if channel == "#kicktest" && reason == "behave")
        })
        .await;

    relay
        .with_events(|events| {
            let kick_ev = events.iter().find(
                |ev| matches!(ev, RelayEvent::Kick { channel, .. } if channel == "#kicktest"),
            );
            assert!(kick_ev.is_some(), "expected Kick event for #kicktest");
            if let Some(RelayEvent::Kick {
                channel, reason, ..
            }) = kick_ev
            {
                assert_eq!(channel, "#kicktest");
                assert_eq!(reason, "behave");
            }
        })
        .await;
}

#[tokio::test]
async fn unexpected_disconnect_publishes_quit_event_to_relay() {
    let relay = Arc::new(TestRelay::new());
    let state = test_state(relay.clone());

    let client = connect_client(&state, "vanisher").await;
    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::ClientIntro { .. }))
        .await;

    // Disconnect without sending QUIT.
    client.disconnect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    relay
        .wait_for_event(|ev| matches!(ev, RelayEvent::Quit { .. }))
        .await;
}
