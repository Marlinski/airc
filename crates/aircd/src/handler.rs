//! IRC command dispatch — handles all commands for registered clients.
//!
//! Each command has its own focused handler function. The top-level
//! [`handle_command`] dispatches based on the parsed [`Command`] variant.

use std::time::{SystemTime, UNIX_EPOCH};

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};
use tracing::debug;

use crate::client::ClientId;
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
        Command::Silence => handle_silence(state, client_id, msg).await,
        Command::Friend => handle_friend(state, client_id, msg).await,
        Command::Motd => handle_motd(state, client_id).await,
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
            client.send_message(&nick_msg);

            let peers = state.peers_in_shared_channels(client_id).await;
            for peer in &peers {
                peer.send_message(&nick_msg);
            }

            // Log nick change to all shared channels.
            let channels = state.channels_for_client(client_id).await;
            for ch in &channels {
                state.logger().log_nick_change(ch, &old_prefix, new_nick);
            }

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

        // ChanServ access check: look up reputation and verify join is allowed.
        let reputation = state
            .services()
            .nickserv
            .get_identity(&client.info.nick)
            .await
            .map(|id| id.reputation)
            .unwrap_or(0);

        if let Err(reason) = state
            .services()
            .chanserv
            .check_join(channel_name, &client.info.nick, reputation)
            .await
        {
            client.send_numeric(ERR_BANNEDFROMCHAN, &[channel_name, &reason]);
            continue;
        }

        let (channel, members) = state.join_channel(client_id, channel_name).await;

        // Broadcast JOIN to all members (including the joiner).
        let join_msg = IrcMessage::join(&channel.name).with_prefix(client.prefix());
        for member in &members {
            member.send_message(&join_msg);
        }

        // Send topic to the joining client.
        send_topic_to_client(&client, &channel.name, &channel.topic);

        // Send NAMES list to the joining client.
        send_names_to_client(state, &client, &channel.name).await;

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
                // Notify the parting client.
                client.send_message(&part_msg);
                // Notify remaining members.
                for member in &remaining {
                    member.send_message(&part_msg);
                }
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

    // Check if the target is a service bot (NickServ, ChanServ, etc.).
    if cmd == Command::Privmsg
        && state
            .services()
            .try_route(state, &client, target, text)
            .await
    {
        return;
    }

    let outgoing = match cmd {
        Command::Notice => IrcMessage::notice(target, text),
        _ => IrcMessage::privmsg(target, text),
    }
    .with_prefix(client.prefix());

    if is_channel_name(target) {
        // Channel message — fan out to all members except sender.
        match state.channel_members_except(target, client_id).await {
            Some(members) => {
                for member in &members {
                    // Skip delivery if the recipient has silenced the sender.
                    if state.is_silenced_by(client_id, member.id).await {
                        continue;
                    }
                    member.send_message(&outgoing);
                }
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
        match state.find_client_by_nick(target).await {
            Some(target_client) => {
                // Block delivery if the recipient has silenced the sender.
                if state.is_silenced_by(client_id, target_client.id).await {
                    // Silently drop — sender gets no error (they're ghosted).
                    return;
                }
                target_client.send_message(&outgoing);
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
        for peer in &peers {
            peer.send_message(&quit_msg);
        }
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
            if !ch.is_member(client_id) {
                client.send_numeric(
                    ERR_NOTONCHANNEL,
                    &[channel_name, "You're not on that channel"],
                );
                return;
            }
            if ch.modes.topic_locked && !ch.is_operator(client_id) {
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
                for member in &members {
                    member.send_message(&topic_msg);
                }
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
        // User mode — just echo back current modes for now.
        let mode_msg = IrcMessage {
            prefix: Some(state.server_name().to_string()),
            command: Command::Numeric(221), // RPL_UMODEIS
            params: vec![client.info.nick.clone(), "+".to_string()],
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

                if let Some(target) = state.find_client_by_nick(target_nick).await {
                    state
                        .set_channel_operator(channel_name, target.id, setting)
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
                        for member in &members {
                            member.send_message(&mode_msg);
                        }
                    }
                } else {
                    client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick"]);
                }
            }
            'i' | 't' | 'n' => {
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
                    for member in &members {
                        member.send_message(&mode_msg);
                    }
                }
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
                    for member in &members {
                        member.send_message(&mode_msg);
                    }
                }
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
                    for member in &members {
                        member.send_message(&mode_msg);
                    }
                }
            }
            _ => {}
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
        if let Some(members) = state.channel_members(mask).await {
            for member in &members {
                // RPL_WHOREPLY: <channel> <user> <host> <server> <nick> <H|G>[*][@|+] :<hopcount> <realname>
                client.send_numeric(
                    RPL_WHOREPLY,
                    &[
                        mask,
                        &member.info.username,
                        &member.info.hostname,
                        state.server_name(),
                        &member.info.nick,
                        "H",
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
            // RPL_WHOISCHANNELS
            let channels = state.channels_for_client(target.id).await;
            if !channels.is_empty() {
                let chan_list = channels.join(" ");
                client.send_numeric(RPL_WHOISCHANNELS, &[&target.info.nick, &chan_list]);
            }
            // Reputation (via NickServ identity lookup).
            if let Some(identity) = state
                .services()
                .nickserv
                .get_identity(&target.info.nick)
                .await
            {
                let rep_line = format!("reputation: {}", identity.reputation);
                client.send_numeric(RPL_WHOISSPECIAL, &[&target.info.nick, &rep_line]);
            }
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

    let channels = state.list_channels().await;
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
        for member in &members {
            member.send_message(&kick_msg);
        }
    }

    state.kick_from_channel(channel_name, target.id).await;
    state.logger().log_kick(
        channel_name,
        &target.info.nick,
        &format!("by {} ({})", client.info.nick, reason),
    );
    debug!(client_id = %client_id, target = %target_nick, channel = %channel_name, "kicked");
}

// ---------------------------------------------------------------------------
// SILENCE — server-side message filtering (+nick / -nick / list)
// ---------------------------------------------------------------------------

async fn handle_silence(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    // No params → list currently silenced nicks.
    let Some(param) = msg.params.first() else {
        let silenced_ids = state.get_silence_list(client_id).await;
        if silenced_ids.is_empty() {
            let notice = IrcMessage::notice(&client.info.nick, "Your silence list is empty")
                .with_prefix(state.server_name());
            client.send_message(&notice);
        } else {
            for tid in &silenced_ids {
                if let Some(target) = state.get_client(*tid).await {
                    let notice = IrcMessage::notice(
                        &client.info.nick,
                        &format!("SILENCE +{}", target.info.nick),
                    )
                    .with_prefix(state.server_name());
                    client.send_message(&notice);
                }
            }
            let notice = IrcMessage::notice(&client.info.nick, "End of silence list")
                .with_prefix(state.server_name());
            client.send_message(&notice);
        }
        return;
    };

    // Parse +nick or -nick.
    let (adding, target_nick) = if let Some(nick) = param.strip_prefix('+') {
        (true, nick)
    } else if let Some(nick) = param.strip_prefix('-') {
        (false, nick)
    } else {
        // Bare nick treated as +nick (add to silence list).
        (true, param.as_str())
    };

    if target_nick.is_empty() {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["SILENCE", "Not enough parameters"]);
        return;
    }

    // Cannot silence yourself.
    if target_nick.eq_ignore_ascii_case(&client.info.nick) {
        let notice = IrcMessage::notice(&client.info.nick, "You cannot silence yourself")
            .with_prefix(state.server_name());
        client.send_message(&notice);
        return;
    }

    let Some(target) = state.find_client_by_nick(target_nick).await else {
        client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick/channel"]);
        return;
    };

    if adding {
        // SILENCE +nick — add to silence list.
        state.add_silence(client_id, target.id).await;

        // Reputation hit on the silenced person (-1).
        state
            .services()
            .nickserv
            .modify_reputation(&target.info.nick, -1)
            .await;

        // Notify the silencing client (DM only).
        let notice = IrcMessage::notice(
            &client.info.nick,
            &format!("You are now ignoring {}", target.info.nick),
        )
        .with_prefix(state.server_name());
        client.send_message(&notice);

        // Notify the silenced person (DM only — no channel broadcast).
        let notice = IrcMessage::notice(
            &target.info.nick,
            &format!("{} is now ignoring you", client.info.nick),
        )
        .with_prefix(state.server_name());
        target.send_message(&notice);

        debug!(
            client_id = %client_id,
            target = %target.info.nick,
            "silence +nick"
        );
    } else {
        // SILENCE -nick — remove from silence list.
        if !state.remove_silence(client_id, target.id).await {
            let notice = IrcMessage::notice(
                &client.info.nick,
                &format!("You are not ignoring {}", target.info.nick),
            )
            .with_prefix(state.server_name());
            client.send_message(&notice);
            return;
        }

        // Notify the client (DM only).
        let notice = IrcMessage::notice(
            &client.info.nick,
            &format!("You are no longer ignoring {}", target.info.nick),
        )
        .with_prefix(state.server_name());
        client.send_message(&notice);

        // Notify the previously silenced person (DM only — no channel broadcast).
        let notice = IrcMessage::notice(
            &target.info.nick,
            &format!("{} is no longer ignoring you", client.info.nick),
        )
        .with_prefix(state.server_name());
        target.send_message(&notice);

        debug!(
            client_id = %client_id,
            target = %target.info.nick,
            "silence -nick"
        );
    }
}

// ---------------------------------------------------------------------------
// FRIEND — server-side friend list (+nick / -nick / list)
// ---------------------------------------------------------------------------

async fn handle_friend(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    // No params → list current friends.
    let Some(param) = msg.params.first() else {
        let friend_ids = state.get_friend_list(client_id).await;
        if friend_ids.is_empty() {
            let notice = IrcMessage::notice(&client.info.nick, "Your friend list is empty")
                .with_prefix(state.server_name());
            client.send_message(&notice);
        } else {
            for fid in &friend_ids {
                if let Some(friend) = state.get_client(*fid).await {
                    let notice = IrcMessage::notice(
                        &client.info.nick,
                        &format!("FRIEND +{}", friend.info.nick),
                    )
                    .with_prefix(state.server_name());
                    client.send_message(&notice);
                }
            }
            let notice = IrcMessage::notice(&client.info.nick, "End of friend list")
                .with_prefix(state.server_name());
            client.send_message(&notice);
        }
        return;
    };

    // Parse +nick or -nick.
    let (adding, target_nick) = if let Some(nick) = param.strip_prefix('+') {
        (true, nick)
    } else if let Some(nick) = param.strip_prefix('-') {
        (false, nick)
    } else {
        // Bare nick treated as +nick (add friend).
        (true, param.as_str())
    };

    if target_nick.is_empty() {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["FRIEND", "Not enough parameters"]);
        return;
    }

    // Cannot friend yourself.
    if target_nick.eq_ignore_ascii_case(&client.info.nick) {
        let notice = IrcMessage::notice(&client.info.nick, "You cannot friend yourself")
            .with_prefix(state.server_name());
        client.send_message(&notice);
        return;
    }

    let Some(target) = state.find_client_by_nick(target_nick).await else {
        client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick/channel"]);
        return;
    };

    if adding {
        // FRIEND +nick — add to friend list.
        state.add_friend(client_id, target.id).await;

        // Reputation boost on the friended person (+1).
        state
            .services()
            .nickserv
            .modify_reputation(&target.info.nick, 1)
            .await;

        // Notify the befriending client (DM only).
        let notice = IrcMessage::notice(
            &client.info.nick,
            &format!("{} is now your friend", target.info.nick),
        )
        .with_prefix(state.server_name());
        client.send_message(&notice);

        // Notify the friended person (DM only).
        let notice = IrcMessage::notice(
            &target.info.nick,
            &format!("{} added you as a friend", client.info.nick),
        )
        .with_prefix(state.server_name());
        target.send_message(&notice);

        debug!(
            client_id = %client_id,
            target = %target.info.nick,
            "friend +nick"
        );
    } else {
        // FRIEND -nick — remove from friend list.
        if !state.remove_friend(client_id, target.id).await {
            let notice = IrcMessage::notice(
                &client.info.nick,
                &format!("{} is not in your friend list", target.info.nick),
            )
            .with_prefix(state.server_name());
            client.send_message(&notice);
            return;
        }

        // Notify the client (DM only).
        let notice = IrcMessage::notice(
            &client.info.nick,
            &format!("{} is no longer your friend", target.info.nick),
        )
        .with_prefix(state.server_name());
        client.send_message(&notice);

        // Notify the removed person (DM only).
        let notice = IrcMessage::notice(
            &target.info.nick,
            &format!("{} removed you as a friend", client.info.nick),
        )
        .with_prefix(state.server_name());
        target.send_message(&notice);

        debug!(
            client_id = %client_id,
            target = %target.info.nick,
            "friend -nick"
        );
    }
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
        // RPL_NAMREPLY: = <channel> :<nicks>
        client.send_numeric(RPL_NAMREPLY, &["=", channel_name, &names_str]);
    }
    client.send_numeric(RPL_ENDOFNAMES, &[channel_name, "End of /NAMES list"]);
}
