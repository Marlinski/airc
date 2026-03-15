//! MODE — channel and user mode handling.

use std::sync::Arc;

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};

use crate::client::ClientId;
use crate::state::SharedState;

use super::is_channel_name;

pub async fn handle_mode(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
        // User mode — only allow self-targeting.
        let target_nick = target.clone();
        if !client.info.nick.eq_ignore_ascii_case(&target_nick) {
            // Silently ignore attempts to set modes on other users.
            return;
        }

        if let Some(mode_str) = msg.params.get(1) {
            let mut setting = true;
            for ch in mode_str.chars() {
                match ch {
                    '+' => setting = true,
                    '-' => setting = false,
                    'i' => {
                        if setting {
                            state.add_user_mode(client_id, 'i').await;
                        } else {
                            state.remove_user_mode(client_id, 'i').await;
                        }
                        // Echo the mode change back to the client.
                        let flag = if setting { "+i" } else { "-i" };
                        let mode_msg = IrcMessage {
                            prefix: Some(client.prefix()),
                            command: Command::Mode,
                            params: vec![client.info.nick.clone(), flag.to_string()],
                        };
                        client.send_message(&mode_msg);
                    }
                    _ => {
                        // Other user modes are not yet implemented.
                    }
                }
            }
        } else {
            // No mode string — echo back current modes.
            let mode_str = state.user_mode_string(client_id).await;
            let mode_msg = IrcMessage {
                prefix: Some(state.server_name().to_string()),
                command: Command::Numeric(221), // RPL_UMODEIS
                params: vec![client.info.nick.clone(), mode_str],
            };
            client.send_message(&mode_msg);
        }
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
