//! Channel commands: JOIN, PART, TOPIC, NAMES, LIST, KICK, INVITE.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};
use tracing::debug;

use crate::client::ClientId;
use crate::state::SharedState;

use super::{is_channel_name, send_names_to_client, send_topic_to_client};

// ---------------------------------------------------------------------------
// JOIN
// ---------------------------------------------------------------------------

pub async fn handle_join(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
            client.send_line(&line);
            for member in &remaining {
                member.send_line(&line);
            }
            state.relay_publish(&part_msg).await;
            state.logger().log_part(&channel_name, &client.info.nick, "");
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

        // Check channel key (+k).
        if let Some(channel) = state.get_channel(channel_name).await
            && let Some(ref chan_key) = channel.modes.key
        {
            match provided_key {
                Some(k) if k == chan_key => {}
                _ => {
                    client.send_numeric(
                        ERR_BADCHANNELKEY,
                        &[channel_name, "Cannot join channel (+k)"],
                    );
                    continue;
                }
            }
        }

        // Check invite-only (+i).
        if let Some(channel) = state.get_channel(channel_name).await {
            if channel.modes.invite_only && !channel.is_invited(&client.info.nick) {
                client.send_numeric(
                    ERR_INVITEONLYCHAN,
                    &[channel_name, "Cannot join channel (+i)"],
                );
                continue;
            }

            // Check member limit (+l).
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

        // ChanServ access check (reputation-gated join).
        if let Some(svc) = state.services() {
            if let Err(reason) = svc.check_join(channel_name, &client.info.nick).await {
                client.send_numeric(ERR_BANNEDFROMCHAN, &[channel_name, &reason]);
                continue;
            }
        }

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

        // Send topic and NAMES to the joining client.
        send_topic_to_client(&client, &channel.name, &channel.topic);
        send_names_to_client(state, &client, &channel.name).await;

        state.relay_publish(&join_msg).await;
        state.logger().log_join(&channel.name, &client.info.nick);
        debug!(client_id = %client_id, channel = %channel.name, "joined channel");
    }
}

// ---------------------------------------------------------------------------
// PART
// ---------------------------------------------------------------------------

pub async fn handle_part(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
                let line: Arc<str> = part_msg.serialize().into();
                client.send_line(&line);
                for member in &remaining {
                    member.send_line(&line);
                }
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
// TOPIC
// ---------------------------------------------------------------------------

pub async fn handle_topic(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
    match state.get_channel(channel_name).await {
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
                state.relay_publish(&topic_msg).await;
                state
                    .logger()
                    .log_topic(channel_name, &client.info.nick, &new_topic);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LIST
// ---------------------------------------------------------------------------

pub async fn handle_list(state: &SharedState, client_id: ClientId, _msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let channels = state.list_channels_for(client_id).await;
    for (name, count, topic) in &channels {
        let count_str = count.to_string();
        let topic_str = topic.as_deref().unwrap_or("");
        client.send_numeric(RPL_LIST, &[name, &count_str, topic_str]);
    }
    client.send_numeric(RPL_LISTEND, &["End of LIST"]);
}

// ---------------------------------------------------------------------------
// NAMES
// ---------------------------------------------------------------------------

pub async fn handle_names(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
// KICK
// ---------------------------------------------------------------------------

pub async fn handle_kick(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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

    if let Some(members) = state.channel_members(channel_name).await {
        let line: Arc<str> = kick_msg.serialize().into();
        for member in &members {
            member.send_line(&line);
        }
    }

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
// INVITE
// ---------------------------------------------------------------------------

pub async fn handle_invite(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    if msg.params.len() < 2 {
        client.send_numeric(ERR_NEEDMOREPARAMS, &["INVITE", "Not enough parameters"]);
        return;
    }

    let target_nick = &msg.params[0];
    let channel_name = &msg.params[1];

    let Some(target) = state.find_client_by_nick(target_nick).await else {
        client.send_numeric(ERR_NOSUCHNICK, &[target_nick, "No such nick/channel"]);
        return;
    };

    // If channel exists, validate membership and permissions.
    if let Some(channel) = state.get_channel(channel_name).await {
        if !channel.is_member_id(client_id) {
            client.send_numeric(
                ERR_NOTONCHANNEL,
                &[channel_name, "You're not on that channel"],
            );
            return;
        }

        if channel.is_member_nick(target_nick) {
            client.send_numeric(
                ERR_USERONCHANNEL,
                &[target_nick, channel_name, "is already on channel"],
            );
            return;
        }

        if channel.modes.invite_only && !channel.is_operator_id(client_id) {
            client.send_numeric(
                ERR_CHANOPRIVSNEEDED,
                &[channel_name, "You're not channel operator"],
            );
            return;
        }
    }

    state.add_channel_invite(channel_name, target_nick).await;

    client.send_numeric(RPL_INVITING, &[&target.info.nick, channel_name]);

    let invite_msg = IrcMessage {
        prefix: Some(client.prefix()),
        command: Command::Invite,
        params: vec![target.info.nick.clone(), channel_name.to_string()],
    };
    target.send_message(&invite_msg);

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
