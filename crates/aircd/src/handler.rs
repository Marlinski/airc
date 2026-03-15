//! IRC command dispatch — handles all commands for registered clients.
//!
//! Each command has its own focused handler function. The top-level
//! [`handle_command`] dispatches based on the parsed [`Command`] variant.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};
use tracing::debug;

use crate::client::{ClientId, ClientKind};
use crate::state::SharedState;

/// Dispatch a parsed IRC command from a registered client.
pub async fn handle_command(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    match &msg.command {
        Command::Nick => handle_nick(state, client_id, msg).await,
        Command::Join => handle_join(state, client_id, msg).await,
        Command::Part => handle_part(state, client_id, msg).await,
        Command::Privmsg => handle_privmsg(state, client_id, msg).await,
        Command::Notice => handle_notice(state, client_id, msg).await,
        Command::Quit => handle_quit(state, client_id, msg).await,
        Command::Ping => handle_ping(state, client_id, msg).await,
        Command::Pong => {} // silently accept
        Command::Topic => handle_topic(state, client_id, msg).await,
        Command::Mode => handle_mode(state, client_id, msg).await,
        Command::Who => handle_who(state, client_id, msg).await,
        Command::Whois => handle_whois(state, client_id, msg).await,
        Command::List => handle_list(state, client_id, msg).await,
        Command::Names => handle_names(state, client_id, msg).await,
        Command::Kick => handle_kick(state, client_id, msg).await,
        Command::Invite => handle_invite(state, client_id, msg).await,
        Command::Away => handle_away(state, client_id, msg).await,
        Command::Ison => handle_ison(state, client_id, msg).await,
        Command::Silence => {
            // SILENCE is handled client-side via NickServ, not by the server.
            if let Some(client) = state.get_client(client_id).await {
                client.send_numeric(ERR_UNKNOWNCOMMAND, &["SILENCE", "Unknown command"]);
            }
        }
        Command::Motd => handle_motd(state, client_id).await,
        Command::Version => handle_version(state, client_id).await,
        Command::Unknown(cmd_str) if cmd_str.eq_ignore_ascii_case("LUSERS") => {
            handle_lusers(state, client_id).await
        }
        Command::Oper => handle_oper(state, client_id, msg).await,
        Command::Kill => handle_kill(state, client_id, msg).await,
        Command::User | Command::Pass => {
            if let Some(client) = state.get_client(client_id).await {
                client.send_numeric(ERR_ALREADYREGISTERED, &["You may not reregister"]);
            }
        }
        _ => {
            if let Some(client) = state.get_client(client_id).await {
                let cmd_str = msg.command.to_string();
                client.send_numeric(ERR_UNKNOWNCOMMAND, &[&cmd_str, "Unknown command"]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NICK — change nickname after registration
// ---------------------------------------------------------------------------

async fn handle_nick(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let Some(new_nick) = msg.params.first() else {
        client.send_numeric(ERR_NONICKNAMEGIVEN, &["No nickname given"]);
        return;
    };

    let old_prefix = client.prefix();

    match state.update_nick(client_id, new_nick).await {
        Ok(()) => {
            // Notify the client and all peers in shared channels.
            let nick_msg = IrcMessage::nick(new_nick).with_prefix(old_prefix.clone());
            let line: Arc<str> = nick_msg.serialize().into();
            client.send_line(&line);

            let peers = state.peers_in_shared_channels(client_id).await;
            for peer in &peers {
                peer.send_line(&line);
            }

            // Log nick change to all shared channels.
            let channels = state.channels_for_client(client_id).await;
            for ch in &channels {
                state.logger().log_nick_change(ch, &old_prefix, new_nick);
            }

            // Relay nick change to remote nodes (they derive nick
            // presence from the NICK message prefix → new nick).
            state.relay_publish(&nick_msg).await;

            debug!(client_id = %client_id, old = %old_prefix, new = %new_nick, "nick change");
        }
        Err(crate::state::NickError::InUse) => {
            client.send_numeric(ERR_NICKNAMEINUSE, &[new_nick, "Nickname is already in use"]);
        }
        Err(crate::state::NickError::Invalid) => {
            client.send_numeric(ERR_ERRONEUSNICKNAME, &[new_nick, "Erroneous nickname"]);
        }
    }
}

// ---------------------------------------------------------------------------
// JOIN — join one or more channels
// ---------------------------------------------------------------------------

async fn handle_join(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let Some(channels_param) = msg.params.first() else {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["JOIN", "Not enough parameters"]);
        return;
    };

    // JOIN 0 — part all channels (RFC 2812 §3.2.1).
    if channels_param == "0" {
        let parted = state.part_all_channels(client_id).await;
        for (channel_name, remaining) in parted {
            let part_msg = IrcMessage::part(&channel_name, None).with_prefix(client.prefix());
            let line: Arc<str> = part_msg.serialize().into();
            // Notify the parting client.
            client.send_line(&line);
            // Notify remaining members.
            for member in &remaining {
                member.send_line(&line);
            }
            // Relay PART to remote nodes.
            state.relay_publish(&part_msg).await;
            state
                .logger()
                .log_part(&channel_name, &client.info.nick, "");
        }
        return;
    }

    // RFC 2812: JOIN #chan1,#chan2 key1,key2
    let keys: Vec<&str> = msg
        .params
        .get(1)
        .map(|s| s.split(',').collect())
        .unwrap_or_default();

    for (i, channel_name) in channels_param.split(',').enumerate() {
        let channel_name = channel_name.trim();
        let provided_key = keys.get(i).map(|s| s.trim());

        if !is_channel_name(channel_name) {
            client.send_numeric(ERR_NOSUCHCHANNEL, &[channel_name, "Invalid channel name"]);
            continue;
        }

        // Check channel key (+k) before ChanServ checks.
        if let Some(channel) = state.get_channel(channel_name).await
            && let Some(ref chan_key) = channel.modes.key
        {
            match provided_key {
                Some(k) if k == chan_key => {} // correct key
                _ => {
                    client.send_numeric(
                        ERR_BADCHANNELKEY,
                        &[channel_name, "Cannot join channel (+k)"],
                    );
                    continue;
                }
            }
        }

        // Check invite-only (+i) — user must be on the channel's invite list.
        if let Some(channel) = state.get_channel(channel_name).await {
            if channel.modes.invite_only && !channel.is_invited(&client.info.nick) {
                client.send_numeric(
                    ERR_INVITEONLYCHAN,
                    &[channel_name, "Cannot join channel (+i)"],
                );
                continue;
            }

            // Check member limit (+l) — channel must not be at capacity.
            if let Some(limit) = channel.modes.limit
                && channel.member_count() >= limit
            {
                client.send_numeric(
                    ERR_CHANNELISFULL,
                    &[channel_name, "Cannot join channel (+l)"],
                );
                continue;
            }
        }

        // ChanServ access checks removed — now handled by external airc-services.
        // TODO(phase2): Re-add join gating via service protocol extensions.

        let (channel, members) = state.join_channel(client_id, channel_name).await;

        // Clear invite entry now that the user has successfully joined.
        state
            .clear_channel_invite(channel_name, &client.info.nick)
            .await;

        // Broadcast JOIN to all members (including the joiner).
        let join_msg = IrcMessage::join(&channel.name).with_prefix(client.prefix());
        let line: Arc<str> = join_msg.serialize().into();
        for member in &members {
            member.send_line(&line);
        }

        // Send topic to the joining client.
        send_topic_to_client(&client, &channel.name, &channel.topic);

        // Send NAMES list to the joining client.
        send_names_to_client(state, &client, &channel.name).await;

        // Relay JOIN to remote nodes.
        state.relay_publish(&join_msg).await;

        state.logger().log_join(&channel.name, &client.info.nick);
        debug!(client_id = %client_id, channel = %channel.name, "joined channel");
    }
}

// ---------------------------------------------------------------------------
// PART — leave one or more channels
// ---------------------------------------------------------------------------

async fn handle_part(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let Some(channels_param) = msg.params.first() else {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["PART", "Not enough parameters"]);
        return;
    };

    let reason = msg.params.get(1).map(|s| s.as_str());

    for channel_name in channels_param.split(',') {
        let channel_name = channel_name.trim();
        let part_msg = IrcMessage::part(channel_name, reason).with_prefix(client.prefix());

        match state.part_channel(client_id, channel_name).await {
            Some(remaining) => {
                // Serialize once for all recipients.
                let line: Arc<str> = part_msg.serialize().into();
                // Notify the parting client.
                client.send_line(&line);
                // Notify remaining members.
                for member in &remaining {
                    member.send_line(&line);
                }

                // Relay PART to remote nodes.
                state.relay_publish(&part_msg).await;

                state
                    .logger()
                    .log_part(channel_name, &client.info.nick, reason.unwrap_or(""));
                debug!(client_id = %client_id, channel = %channel_name, "parted channel");
            }
            None => {
                client.send_numeric(
                    ERR_NOTONCHANNEL,
                    &[channel_name, "You're not on that channel"],
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PRIVMSG / NOTICE — send message to a channel or user
// ---------------------------------------------------------------------------

async fn handle_privmsg(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    route_message(state, client_id, msg, Command::Privmsg).await;
}

async fn handle_notice(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    route_message(state, client_id, msg, Command::Notice).await;
}

async fn route_message(state: &SharedState, client_id: ClientId, msg: &IrcMessage, cmd: Command) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if msg.params.len() < 2 {
        // NOTICE should not generate error replies per RFC.
        if cmd == Command::Privmsg {
            client.send_numeric(ERR_NEEDMOREPARAMS, &["PRIVMSG", "Not enough parameters"]);
        }
        return;
    }

    let target = &msg.params[0];
    let text = &msg.params[1];

    let outgoing = match cmd {
        Command::Notice => IrcMessage::notice(target, text),
        _ => IrcMessage::privmsg(target, text),
    }
    .with_prefix(client.prefix());

    if is_channel_name(target) {
        // +n enforcement: non-members cannot send to channels with no-external mode.
        if state.channel_is_no_external(target).await
            && !state.is_channel_member(target, &client.info.nick).await
        {
            if cmd == Command::Privmsg {
                client.send_numeric(ERR_CANNOTSENDTOCHAN, &[target, "Cannot send to channel"]);
            }
            return;
        }

        // +m enforcement: only voiced/opped users can speak in moderated channels.
        if !state.can_speak_in_channel(target, &client.info.nick).await {
            if cmd == Command::Privmsg {
                client.send_numeric(ERR_CANNOTSENDTOCHAN, &[target, "Cannot send to channel"]);
            }
            return;
        }

        // Channel message — fan out to all members except sender.
        match state.channel_members_except(target, client_id).await {
            Some(members) => {
                // Serialize once, share the Arc<str> with all recipients.
                let line: Arc<str> = outgoing.serialize().into();

                for member in &members {
                    member.send_line(&line);
                }

                // Relay channel message to remote nodes.
                state.relay_publish(&outgoing).await;

                // Log channel message.
                match cmd {
                    Command::Notice => state.logger().log_notice(target, &client.info.nick, text),
                    _ => state.logger().log_message(target, &client.info.nick, text),
                }
            }
            None => {
                if cmd == Command::Privmsg {
                    client.send_numeric(ERR_NOSUCHCHANNEL, &[target, "No such channel"]);
                }
            }
        }
    } else {
        // Direct message to a user.
        // Check nick_kind to handle both local and remote routing.
        match state.nick_kind(target).await {
            Some(ClientKind::Local(_)) => {
                // Local client — deliver directly.
                if let Some(target_client) = state.find_client_by_nick(target).await {
                    target_client.send_message(&outgoing);

                    // If the target is away and this is a PRIVMSG, send RPL_AWAY to sender.
                    if cmd == Command::Privmsg
                        && let Some(away_msg) = state.get_away_message(target_client.id).await
                    {
                        client.send_numeric(RPL_AWAY, &[&target_client.info.nick, &away_msg]);
                    }

                    // Log DM (keyed by recipient nick).
                    match cmd {
                        Command::Notice => {
                            state.logger().log_notice(target, &client.info.nick, text)
                        }
                        _ => state.logger().log_message(target, &client.info.nick, text),
                    }
                } else if cmd == Command::Privmsg {
                    client.send_numeric(ERR_NOSUCHNICK, &[target, "No such nick/channel"]);
                }
            }
            Some(ClientKind::Remote(_)) => {
                // Remote client — relay the message; the remote node will deliver locally.
                state.relay_publish(&outgoing).await;

                // Log DM (keyed by recipient nick).
                match cmd {
                    Command::Notice => state.logger().log_notice(target, &client.info.nick, text),
                    _ => state.logger().log_message(target, &client.info.nick, text),
                }
            }
            None => {
                if cmd == Command::Privmsg {
                    client.send_numeric(ERR_NOSUCHNICK, &[target, "No such nick/channel"]);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// QUIT — disconnect
// ---------------------------------------------------------------------------

async fn handle_quit(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let reason = msg.params.first().map(|s| s.as_str()).unwrap_or("Quit");
    let client = state.get_client(client_id).await;

    // Log quit to all channels the user is in (before removing them).
    if let Some(ref client) = client {
        let channels = state.channels_for_client(client_id).await;
        for ch in &channels {
            state.logger().log_quit(ch, &client.info.nick, reason);
        }
    }

    // Notify all peers in shared channels.
    let peers = state.peers_in_shared_channels(client_id).await;
    if let Some(ref client) = client {
        let quit_msg = IrcMessage::quit(Some(reason)).with_prefix(client.prefix());
        let line: Arc<str> = quit_msg.serialize().into();
        for peer in &peers {
            peer.send_line(&line);
        }

        // Relay QUIT to remote nodes (they derive nick removal from this).
        state.relay_publish(&quit_msg).await;
    }

    // Send ERROR to the quitting client.
    if let Some(ref client) = client {
        let error_msg = IrcMessage {
            prefix: None,
            command: Command::Unknown("ERROR".to_string()),
            params: vec![format!(
                "Closing Link: {} (Quit: {})",
                client.info.hostname, reason
            )],
        };
        client.send_message(&error_msg);
    }

    state.remove_client(client_id).await;
    debug!(client_id = %client_id, reason = %reason, "client quit");
}

// ---------------------------------------------------------------------------
// PING — keepalive
// ---------------------------------------------------------------------------

async fn handle_ping(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };
    let token = msg.params.first().map(|s| s.as_str()).unwrap_or("");
    let pong = IrcMessage::pong(token).with_prefix(state.server_name());
    client.send_message(&pong);
}

// ---------------------------------------------------------------------------
// TOPIC — get or set channel topic
// ---------------------------------------------------------------------------

async fn handle_topic(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let Some(channel_name) = msg.params.first() else {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["TOPIC", "Not enough parameters"]);
        return;
    };

    // Query topic.
    if msg.params.len() < 2 {
        match state.get_channel(channel_name).await {
            Some(channel) => send_topic_to_client(&client, channel_name, &channel.topic),
            None => {
                client.send_numeric(ERR_NOSUCHCHANNEL, &[channel_name, "No such channel"]);
            }
        }
        return;
    }

    // Set topic — check membership and permissions.
    let channel = state.get_channel(channel_name).await;
    match channel {
        None => {
            client.send_numeric(ERR_NOSUCHCHANNEL, &[channel_name, "No such channel"]);
        }
        Some(ch) => {
            if !ch.is_member_id(client_id) {
                client.send_numeric(
                    ERR_NOTONCHANNEL,
                    &[channel_name, "You're not on that channel"],
                );
                return;
            }
            if ch.modes.topic_locked && !ch.is_operator_id(client_id) {
                client.send_numeric(
                    ERR_CHANOPRIVSNEEDED,
                    &[channel_name, "You're not channel operator"],
                );
                return;
            }

            let new_topic = msg.params[1].clone();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            if let Some(members) = state
                .set_channel_topic(
                    channel_name,
                    new_topic.clone(),
                    client.info.nick.clone(),
                    now,
                )
                .await
            {
                let topic_msg = IrcMessage {
                    prefix: Some(client.prefix()),
                    command: Command::Topic,
                    params: vec![channel_name.to_string(), new_topic.clone()],
                };
                let line: Arc<str> = topic_msg.serialize().into();
                for member in &members {
                    member.send_line(&line);
                }

                // Relay TOPIC change to remote nodes.
                state.relay_publish(&topic_msg).await;

                state
                    .logger()
                    .log_topic(channel_name, &client.info.nick, &new_topic);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MODE — channel and user modes
// ---------------------------------------------------------------------------

async fn handle_mode(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let Some(target) = msg.params.first() else {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["MODE", "Not enough parameters"]);
        return;
    };

    if is_channel_name(target) {
        handle_channel_mode(state, &client, client_id, target, msg).await;
    } else {
        // User mode — echo back current modes.
        let mode_str = state.user_mode_string(client_id).await;
        let mode_msg = IrcMessage {
            prefix: Some(state.server_name().to_string()),
            command: Command::Numeric(221), // RPL_UMODEIS
            params: vec![client.info.nick.clone(), mode_str],
        };
        client.send_message(&mode_msg);
    }
}

async fn handle_channel_mode(
    state: &SharedState,
    client: &crate::client::ClientHandle,
    client_id: ClientId,
    channel_name: &str,
    msg: &IrcMessage,
) {
    // Query mode — no mode string provided.
    if msg.params.len() < 2 {
        match state.channel_mode_string(channel_name).await {
            Some(mode_str) => {
                client.send_numeric(RPL_CHANNELMODEIS, &[channel_name, &mode_str]);
                // RPL_CREATIONTIME (329) — send channel creation timestamp.
                if let Some(created_at) = state.channel_created_at(channel_name).await {
                    client.send_numeric(RPL_CREATIONTIME, &[channel_name, &created_at.to_string()]);
                }
            }
            None => {
                client.send_numeric(ERR_NOSUCHCHANNEL, &[channel_name, "No such channel"]);
            }
        }
        return;
    }

    let mode_str = &msg.params[1];
    let mut param_idx = 2;
    let mut setting = true;

    for ch in mode_str.chars() {
        match ch {
            '+' => setting = true,
            '-' => setting = false,
            'o' => {
                // Op/deop a user.
                let Some(target_nick) = msg.params.get(param_idx) else {
                    client.send_numeric(ERR_NEEDMOREPARAMS, &["MODE", "Not enough parameters"]);
                    return;
                };
                param_idx += 1;

                if !state.is_channel_operator(channel_name, client_id).await {
                    client.send_numeric(
                        ERR_CHANOPRIVSNEEDED,
                        &[channel_name, "You're not channel operator"],
                    );
                    return;
                }

                if let Some(_target) = state.find_client_by_nick(target_nick).await {
                    // Check target is on the channel.
                    if !state.is_channel_member(channel_name, target_nick).await {
                        client.send_numeric(
                            ERR_USERNOTINCHANNEL,
                            &[target_nick, channel_name, "They aren't on that channel"],
                        );
                        return;
                    }

                    state
                        .set_channel_operator(channel_name, target_nick, setting)
                        .await;

                    // Broadcast mode change.
                    let mode_change = if setting {
                        format!("+o {target_nick}")
                    } else {
                        format!("-o {target_nick}")
                    };
                    let mode_msg = IrcMessage {
                        prefix: Some(client.prefix()),
                        command: Command::Mode,
                        params: vec![channel_name.to_string(), mode_change],
                    };
                    if let Some(members) = state.channel_members(channel_name).await {
                        let line: Arc<str> = mode_msg.serialize().into();
                        for member in &members {
                            member.send_line(&line);
                        }
                    }

                    // Relay MODE change to remote nodes.
                    state.relay_publish(&mode_msg).await;
                } else {
                    client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick"]);
                }
            }
            'i' | 't' | 'n' | 'm' | 's' => {
                if !state.is_channel_operator(channel_name, client_id).await {
                    client.send_numeric(
                        ERR_CHANOPRIVSNEEDED,
                        &[channel_name, "You're not channel operator"],
                    );
                    return;
                }
                state
                    .set_channel_mode(channel_name, ch, setting, None)
                    .await;

                let flag = if setting {
                    format!("+{ch}")
                } else {
                    format!("-{ch}")
                };
                let mode_msg = IrcMessage {
                    prefix: Some(client.prefix()),
                    command: Command::Mode,
                    params: vec![channel_name.to_string(), flag],
                };
                if let Some(members) = state.channel_members(channel_name).await {
                    let line: Arc<str> = mode_msg.serialize().into();
                    for member in &members {
                        member.send_line(&line);
                    }
                }

                // Relay MODE change to remote nodes.
                state.relay_publish(&mode_msg).await;
            }
            'v' => {
                // Voice/devoice a user.
                let Some(target_nick) = msg.params.get(param_idx) else {
                    client.send_numeric(ERR_NEEDMOREPARAMS, &["MODE", "Not enough parameters"]);
                    return;
                };
                param_idx += 1;

                if !state.is_channel_operator(channel_name, client_id).await {
                    client.send_numeric(
                        ERR_CHANOPRIVSNEEDED,
                        &[channel_name, "You're not channel operator"],
                    );
                    return;
                }

                // Check target is on the channel.
                if !state.is_channel_member(channel_name, target_nick).await {
                    client.send_numeric(
                        ERR_USERNOTINCHANNEL,
                        &[target_nick, channel_name, "They aren't on that channel"],
                    );
                    return;
                }

                state
                    .set_channel_voice(channel_name, target_nick, setting)
                    .await;

                // Broadcast mode change.
                let mode_change = if setting {
                    format!("+v {target_nick}")
                } else {
                    format!("-v {target_nick}")
                };
                let mode_msg = IrcMessage {
                    prefix: Some(client.prefix()),
                    command: Command::Mode,
                    params: vec![channel_name.to_string(), mode_change],
                };
                if let Some(members) = state.channel_members(channel_name).await {
                    let line: Arc<str> = mode_msg.serialize().into();
                    for member in &members {
                        member.send_line(&line);
                    }
                }

                // Relay MODE change to remote nodes.
                state.relay_publish(&mode_msg).await;
            }
            'k' => {
                if !state.is_channel_operator(channel_name, client_id).await {
                    client.send_numeric(
                        ERR_CHANOPRIVSNEEDED,
                        &[channel_name, "You're not channel operator"],
                    );
                    return;
                }
                let param = if setting {
                    let Some(p) = msg.params.get(param_idx) else {
                        client.send_numeric(ERR_NEEDMOREPARAMS, &["MODE", "Not enough parameters"]);
                        return;
                    };
                    param_idx += 1;
                    Some(p.as_str())
                } else {
                    None
                };
                state
                    .set_channel_mode(channel_name, 'k', setting, param)
                    .await;

                // Broadcast: show key on +k, hide on -k (RFC 2812: key is
                // visible to channel members in the MODE message).
                let mode_change = if setting {
                    format!("+k {}", param.unwrap_or("*"))
                } else {
                    "-k".to_string()
                };
                let mode_msg = IrcMessage {
                    prefix: Some(client.prefix()),
                    command: Command::Mode,
                    params: vec![channel_name.to_string(), mode_change],
                };
                if let Some(members) = state.channel_members(channel_name).await {
                    let line: Arc<str> = mode_msg.serialize().into();
                    for member in &members {
                        member.send_line(&line);
                    }
                }

                // Relay MODE change to remote nodes.
                state.relay_publish(&mode_msg).await;
            }
            'l' => {
                if !state.is_channel_operator(channel_name, client_id).await {
                    client.send_numeric(
                        ERR_CHANOPRIVSNEEDED,
                        &[channel_name, "You're not channel operator"],
                    );
                    return;
                }
                let param = if setting {
                    let p = msg.params.get(param_idx).map(|s| s.as_str());
                    param_idx += 1;
                    p
                } else {
                    None
                };
                state
                    .set_channel_mode(channel_name, 'l', setting, param)
                    .await;

                let mode_change = if setting {
                    format!("+l {}", param.unwrap_or("0"))
                } else {
                    "-l".to_string()
                };
                let mode_msg = IrcMessage {
                    prefix: Some(client.prefix()),
                    command: Command::Mode,
                    params: vec![channel_name.to_string(), mode_change],
                };
                if let Some(members) = state.channel_members(channel_name).await {
                    let line: Arc<str> = mode_msg.serialize().into();
                    for member in &members {
                        member.send_line(&line);
                    }
                }

                // Relay MODE change to remote nodes.
                state.relay_publish(&mode_msg).await;
            }
            unknown => {
                let mode_str = unknown.to_string();
                client.send_numeric(ERR_UNKNOWNMODE, &[&mode_str, "is unknown mode char to me"]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WHO — list users matching a pattern
// ---------------------------------------------------------------------------

async fn handle_who(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let mask = msg.params.first().map(|s| s.as_str()).unwrap_or("*");

    if is_channel_name(mask) {
        if let Some(channel) = state.get_channel(mask).await {
            let members = state.channel_members(mask).await.unwrap_or_default();
            for member in &members {
                // H = here, G = gone (away).
                let away_flag = if state.get_away_message(member.id).await.is_some() {
                    "G"
                } else {
                    "H"
                };
                // Membership prefix: @ for op, + for voiced.
                let nick_lower = member.info.nick.to_ascii_lowercase();
                let prefix = if channel.operators.contains(&nick_lower) {
                    "@"
                } else if channel.voiced.contains(&nick_lower) {
                    "+"
                } else {
                    ""
                };
                let flags = format!("{away_flag}{prefix}");
                // RPL_WHOREPLY: <channel> <user> <host> <server> <nick> <H|G>[*][@|+] :<hopcount> <realname>
                client.send_numeric(
                    RPL_WHOREPLY,
                    &[
                        mask,
                        &member.info.username,
                        &member.info.hostname,
                        state.server_name(),
                        &member.info.nick,
                        &flags,
                        &format!("0 {}", member.info.realname),
                    ],
                );
            }
        }
    }

    client.send_numeric(RPL_ENDOFWHO, &[mask, "End of WHO list"]);
}

// ---------------------------------------------------------------------------
// WHOIS — query user info
// ---------------------------------------------------------------------------

async fn handle_whois(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let Some(target_nick) = msg.params.first() else {
        client.send_numeric(ERR_NONICKNAMEGIVEN, &["No nickname given"]);
        return;
    };

    match state.find_client_by_nick(target_nick).await {
        Some(target) => {
            // RPL_WHOISUSER: <nick> <user> <host> * :<realname>
            client.send_numeric(
                RPL_WHOISUSER,
                &[
                    &target.info.nick,
                    &target.info.username,
                    &target.info.hostname,
                    "*",
                    &target.info.realname,
                ],
            );
            // RPL_WHOISSERVER
            client.send_numeric(
                RPL_WHOISSERVER,
                &[&target.info.nick, state.server_name(), "AIRC server"],
            );
            // RPL_WHOISOPERATOR — if target is an IRC operator.
            if target.info.is_oper() {
                let oper_text = if target.info.is_service() {
                    "is a service"
                } else {
                    "is an IRC operator"
                };
                client.send_numeric(RPL_WHOISOPERATOR, &[&target.info.nick, oper_text]);
            }
            // RPL_AWAY — if target is away.
            if let Some(away_msg) = state.get_away_message(target.id).await {
                client.send_numeric(RPL_AWAY, &[&target.info.nick, &away_msg]);
            }
            // RPL_WHOISCHANNELS — filtered by +s visibility.
            let channels = state
                .channels_for_client_seen_by(target.id, client_id)
                .await;
            if !channels.is_empty() {
                let chan_list = channels.join(" ");
                client.send_numeric(RPL_WHOISCHANNELS, &[&target.info.nick, &chan_list]);
            }
            // Reputation (via NickServ identity lookup) removed — now handled
            // by external airc-services.
            // TODO(phase2): Re-add reputation in WHOIS via service protocol extensions.

            // RPL_ENDOFWHOIS
            client.send_numeric(RPL_ENDOFWHOIS, &[&target.info.nick, "End of WHOIS list"]);
        }
        None => {
            client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick/channel"]);
        }
    }
}

// ---------------------------------------------------------------------------
// LIST — list all channels
// ---------------------------------------------------------------------------

async fn handle_list(state: &SharedState, client_id: ClientId, _msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let channels = state.list_channels_for(client_id).await;
    for (name, count, topic) in &channels {
        let count_str = count.to_string();
        let topic_str = topic.as_deref().unwrap_or("");
        client.send_numeric(RPL_LIST, &[&name, &count_str, topic_str]);
    }
    client.send_numeric(RPL_LISTEND, &["End of LIST"]);
}

// ---------------------------------------------------------------------------
// NAMES — list users in a channel
// ---------------------------------------------------------------------------

async fn handle_names(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if let Some(channel_name) = msg.params.first() {
        for ch in channel_name.split(',') {
            send_names_to_client(state, &client, ch.trim()).await;
        }
    }
}

// ---------------------------------------------------------------------------
// KICK — remove a user from a channel
// ---------------------------------------------------------------------------

async fn handle_kick(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if msg.params.len() < 2 {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["KICK", "Not enough parameters"]);
        return;
    }

    let channel_name = &msg.params[0];
    let target_nick = &msg.params[1];
    let reason = msg.params.get(2).map(|s| s.as_str()).unwrap_or(target_nick);

    // Check permissions.
    if !state.is_channel_operator(channel_name, client_id).await {
        client.send_numeric(
            ERR_CHANOPRIVSNEEDED,
            &[channel_name, "You're not channel operator"],
        );
        return;
    }

    let Some(target) = state.find_client_by_nick(target_nick).await else {
        client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick"]);
        return;
    };

    // Check target is actually on the channel.
    if !state.is_channel_member(channel_name, target_nick).await {
        client.send_numeric(
            ERR_USERNOTINCHANNEL,
            &[target_nick, channel_name, "They aren't on that channel"],
        );
        return;
    }

    // Broadcast KICK before removing.
    let kick_msg = IrcMessage {
        prefix: Some(client.prefix()),
        command: Command::Kick,
        params: vec![
            channel_name.to_string(),
            target.info.nick.clone(),
            reason.to_string(),
        ],
    };

    // Send to all current members (including the target).
    if let Some(members) = state.channel_members(channel_name).await {
        let line: Arc<str> = kick_msg.serialize().into();
        for member in &members {
            member.send_line(&line);
        }
    }

    // Relay KICK to remote nodes.
    state.relay_publish(&kick_msg).await;

    state.kick_from_channel(channel_name, target_nick).await;
    state.logger().log_kick(
        channel_name,
        &target.info.nick,
        &format!("by {} ({})", client.info.nick, reason),
    );
    debug!(client_id = %client_id, target = %target_nick, channel = %channel_name, "kicked");
}

// ---------------------------------------------------------------------------
// INVITE — invite a user to a channel (RFC 2812 §3.2.7)
// ---------------------------------------------------------------------------

async fn handle_invite(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if msg.params.len() < 2 {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["INVITE", "Not enough parameters"]);
        return;
    }

    let target_nick = &msg.params[0];
    let channel_name = &msg.params[1];

    // Target must exist.
    let Some(target) = state.find_client_by_nick(target_nick).await else {
        client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick/channel"]);
        return;
    };

    // If channel exists, validate membership and permissions.
    if let Some(channel) = state.get_channel(channel_name).await {
        // Inviter must be on the channel.
        if !channel.is_member_id(client_id) {
            client.send_numeric(
                ERR_NOTONCHANNEL,
                &[channel_name, "You're not on that channel"],
            );
            return;
        }

        // Target must not already be on the channel.
        if channel.is_member_nick(target_nick) {
            client.send_numeric(
                ERR_USERONCHANNEL,
                &[target_nick, channel_name, "is already on channel"],
            );
            return;
        }

        // If channel is +i, only operators may invite.
        if channel.modes.invite_only && !channel.is_operator_id(client_id) {
            client.send_numeric(
                ERR_CHANOPRIVSNEEDED,
                &[channel_name, "You're not channel operator"],
            );
            return;
        }
    }

    // Add target to the channel's invite list.
    state.add_channel_invite(channel_name, target_nick).await;

    // RPL_INVITING to the inviter.
    client.send_numeric(RPL_INVITING, &[&target.info.nick, channel_name]);

    // Send INVITE message to the invitee.
    let invite_msg = IrcMessage {
        prefix: Some(client.prefix()),
        command: Command::Invite,
        params: vec![target.info.nick.clone(), channel_name.to_string()],
    };
    target.send_message(&invite_msg);

    // If invitee is away, send RPL_AWAY to inviter.
    if let Some(away_msg) = state.get_away_message(target.id).await {
        client.send_numeric(RPL_AWAY, &[&target.info.nick, &away_msg]);
    }

    debug!(
        client_id = %client_id,
        target = %target_nick,
        channel = %channel_name,
        "invite"
    );
}

// ---------------------------------------------------------------------------
// AWAY — set or clear away status (RFC 2812 §4.1)
// ---------------------------------------------------------------------------

async fn handle_away(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if let Some(away_text) = msg.params.first()
        && !away_text.is_empty()
    {
        // Set away.
        state.set_away(client_id, away_text.clone()).await;
        client.send_numeric(RPL_NOWAWAY, &["You have been marked as being away"]);
        return;
    }

    // Clear away (no params or empty param).
    state.clear_away(client_id).await;
    client.send_numeric(RPL_UNAWAY, &["You are no longer marked as being away"]);
}

// ---------------------------------------------------------------------------
// ISON — lightweight presence check (RFC 2812 §4.9)
// ---------------------------------------------------------------------------

async fn handle_ison(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let mut online_nicks = Vec::new();
    for nick in &msg.params {
        // Each param may be a single nick (standard) or space-separated
        // (some clients pack them into the trailing param).
        for n in nick.split_whitespace() {
            if state.find_client_by_nick(n).await.is_some() {
                online_nicks.push(n.to_string());
            }
        }
    }

    let reply = online_nicks.join(" ");
    client.send_numeric(RPL_ISON, &[&reply]);
}

// ---------------------------------------------------------------------------
// MOTD — message of the day
// ---------------------------------------------------------------------------

async fn handle_motd(state: &SharedState, client_id: ClientId) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };
    send_motd(state, &client);
}

/// Send the MOTD to a client. Called on registration and on `MOTD` command.
pub fn send_motd(state: &SharedState, client: &crate::client::ClientHandle) {
    let config = state.config();
    client.send_numeric(
        RPL_MOTDSTART,
        &[&format!("- {} Message of the day -", config.server_name)],
    );
    for line in &config.motd {
        client.send_numeric(RPL_MOTD, &[&format!("- {line}")]);
    }
    client.send_numeric(RPL_ENDOFMOTD, &["End of MOTD command"]);
}

// ---------------------------------------------------------------------------
// OPER — authenticate as an IRC operator (RFC 2812 §3.1.4)
// ---------------------------------------------------------------------------

async fn handle_oper(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if msg.params.len() < 2 {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["OPER", "Not enough parameters"]);
        return;
    }

    let name = &msg.params[0];
    let password = &msg.params[1];

    let config = state.config();
    let oper_entry = config
        .operators
        .iter()
        .find(|o| o.name == *name && o.password == *password);

    match oper_entry {
        Some(entry) => {
            let is_service = entry.service;

            // Grant +o (and +S if service).
            state.add_user_mode(client_id, 'o').await;
            if is_service {
                state.add_user_mode(client_id, 'S').await;
            }

            // RPL_YOUREOPER (381).
            client.send_numeric(RPL_YOUREOPER, &["You are now an IRC operator"]);

            // Notify the user of their new modes.
            let mode_str = if is_service { "+oS" } else { "+o" };
            let mode_msg = IrcMessage::mode(&client.info.nick, Some(mode_str))
                .with_prefix(state.server_name());
            client.send_message(&mode_msg);

            debug!(
                client_id = %client_id,
                nick = %client.info.nick,
                service = is_service,
                "client opered up"
            );
        }
        None => {
            // ERR_PASSWDMISMATCH (464) — per RFC 2812, this is sent for
            // wrong name OR wrong password (don't reveal which).
            client.send_numeric(ERR_PASSWDMISMATCH, &["Password incorrect"]);
            debug!(
                client_id = %client_id,
                nick = %client.info.nick,
                oper_name = %name,
                "failed OPER attempt"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// KILL — forcibly disconnect a user (operator/service command, RFC 2812 §3.7.1)
// ---------------------------------------------------------------------------

async fn handle_kill(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if msg.params.is_empty() {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["KILL", "Not enough parameters"]);
        return;
    }

    // Only operators (+o) or services (+S) can use KILL.
    if !client.info.is_oper() && !client.info.is_service() {
        client.send_numeric(
            ERR_NOPRIVILEGES,
            &["Permission Denied- You're not an IRC operator"],
        );
        return;
    }

    let target_nick = &msg.params[0];
    let reason = msg.params.get(1).map(|s| s.as_str()).unwrap_or("Killed");

    // Disconnect the target.
    let Some((target_handle, peers)) = state.force_disconnect(target_nick).await else {
        client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick/channel"]);
        return;
    };

    // Send ERROR to the killed client.
    let error_msg = IrcMessage {
        prefix: None,
        command: Command::Unknown("ERROR".to_string()),
        params: vec![format!(
            "Closing Link: {} ({reason})",
            target_handle.info.hostname
        )],
    };
    target_handle.send_message(&error_msg);

    // Send QUIT to all their channel peers.
    let quit_msg = IrcMessage::quit(Some(reason)).with_prefix(target_handle.prefix());
    let line: Arc<str> = quit_msg.serialize().into();
    for peer in &peers {
        peer.send_line(&line);
    }

    debug!(
        client_id = %client_id,
        nick = %client.info.nick,
        target = %target_nick,
        reason = %reason,
        "KILL: disconnected client"
    );
}

// ---------------------------------------------------------------------------
// VERSION — server version info (RFC 2812 §3.4.3)
// ---------------------------------------------------------------------------

async fn handle_version(state: &SharedState, client_id: ClientId) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };
    // RPL_VERSION: <version> <server> :<comments>
    client.send_numeric(
        RPL_VERSION,
        &["airc-0.1.0", state.server_name(), "AIRC IRC server"],
    );
}

// ---------------------------------------------------------------------------
// LUSERS — connection statistics (RFC 2812 §3.4.2)
// ---------------------------------------------------------------------------

async fn handle_lusers(state: &SharedState, client_id: ClientId) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };
    send_lusers(state, &client).await;
}

/// Send LUSERS numerics (251-255, 265-266) to a client.
pub async fn send_lusers(state: &SharedState, client: &crate::client::ClientHandle) {
    let user_count = state.local_client_count().await;
    let channel_count = state.channel_count().await;
    let oper_count = state.oper_count().await;

    // 251 RPL_LUSERCLIENT: "There are <n> users and 0 services on 1 servers"
    client.send_numeric(
        RPL_LUSERCLIENT,
        &[&format!(
            "There are {user_count} users and 0 services on 1 servers"
        )],
    );
    // 252 RPL_LUSEROP: <count> :operator(s) online
    client.send_numeric(
        RPL_LUSEROP,
        &[&oper_count.to_string(), "operator(s) online"],
    );
    // 253 RPL_LUSERUNKNOWN: <count> :unknown connection(s)
    client.send_numeric(RPL_LUSERUNKNOWN, &["0", "unknown connection(s)"]);
    // 254 RPL_LUSERCHANNELS: <count> :channels formed
    client.send_numeric(
        RPL_LUSERCHANNELS,
        &[&channel_count.to_string(), "channels formed"],
    );
    // 255 RPL_LUSERME: "I have <n> clients and 1 servers"
    client.send_numeric(
        RPL_LUSERME,
        &[&format!("I have {user_count} clients and 1 servers")],
    );
    // 265 RPL_LOCALUSERS
    client.send_numeric(
        RPL_LOCALUSERS,
        &[&format!(
            "Current local users: {user_count}, max: {user_count}"
        )],
    );
    // 266 RPL_GLOBALUSERS
    client.send_numeric(
        RPL_GLOBALUSERS,
        &[&format!(
            "Current global users: {user_count}, max: {user_count}"
        )],
    );
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Whether a string looks like a channel name (`#` or `&` prefix).
fn is_channel_name(s: &str) -> bool {
    airc_shared::validate::is_channel_name(s)
}

/// Send topic info (or "no topic") to a single client.
fn send_topic_to_client(
    client: &crate::client::ClientHandle,
    channel_name: &str,
    topic: &Option<(String, String, u64)>,
) {
    match topic {
        Some((text, setter, timestamp)) => {
            client.send_numeric(RPL_TOPIC, &[channel_name, text]);
            client.send_numeric(
                RPL_TOPICWHOTIME,
                &[channel_name, setter, &timestamp.to_string()],
            );
        }
        None => {
            client.send_numeric(RPL_NOTOPIC, &[channel_name, "No topic is set"]);
        }
    }
}

/// Send the NAMES list for a channel to a single client.
async fn send_names_to_client(
    state: &SharedState,
    client: &crate::client::ClientHandle,
    channel_name: &str,
) {
    if let Some(nicks) = state.channel_nicks_with_prefix(channel_name).await {
        let names_str = nicks.join(" ");
        // Channel type: @ for secret, = for public (no private channels in our impl).
        let chan_type = if state.channel_is_secret(channel_name).await {
            "@"
        } else {
            "="
        };
        // RPL_NAMREPLY: <type> <channel> :<nicks>
        client.send_numeric(RPL_NAMREPLY, &[chan_type, channel_name, &names_str]);
    }
    client.send_numeric(RPL_ENDOFNAMES, &[channel_name, "End of /NAMES list"]);
}
