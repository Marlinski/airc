//! User-level commands: QUIT, PING, WHO, WHOIS, AWAY, ISON.

use std::sync::Arc;

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};
use tracing::debug;

use crate::client::ClientId;
use crate::state::SharedState;

use super::is_channel_name;

// ---------------------------------------------------------------------------
// QUIT — disconnect
// ---------------------------------------------------------------------------

pub async fn handle_quit(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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

pub async fn handle_ping(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };
    let token = msg.params.first().map(|s| s.as_str()).unwrap_or("");
    let pong = IrcMessage::pong(token).with_prefix(state.server_name());
    client.send_message(&pong);
}

// ---------------------------------------------------------------------------
// WHO — list users matching a pattern
// ---------------------------------------------------------------------------

pub async fn handle_who(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let Some(client) = state.get_client(client_id).await else {
        return;
    };

    let mask = msg.params.first().map(|s| s.as_str()).unwrap_or("*");

    if is_channel_name(mask) {
        // Channel WHO — list all visible members.
        // +i members are visible because the querier is (presumably) also on
        // the channel.  Membership check is not strictly required here per
        // RFC 2812 but non-members of a +s channel will see nothing via LIST
        // anyway.
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
    } else {
        // Non-channel WHO (mask = "*" or a nick/host pattern).
        // +i users are hidden from non-channel WHO unless the querier shares
        // a channel with them.
        let all_clients = state.all_clients().await;
        for target in &all_clients {
            // Skip self — some clients expect self to appear, but RFC 2812
            // does not require it; we include self unconditionally.
            let skip_invisible = target.id != client_id
                && target.info.is_invisible()
                && !state.shares_channel(client_id, target.id).await;
            if skip_invisible {
                continue;
            }

            // Simple glob: "*" matches everyone; otherwise match nick.
            if mask != "*" && !target.info.nick.eq_ignore_ascii_case(mask) {
                continue;
            }

            let away_flag = if state.get_away_message(target.id).await.is_some() {
                "G"
            } else {
                "H"
            };
            client.send_numeric(
                RPL_WHOREPLY,
                &[
                    "*",
                    &target.info.username,
                    &target.info.hostname,
                    state.server_name(),
                    &target.info.nick,
                    away_flag,
                    &format!("0 {}", target.info.realname),
                ],
            );
        }
    }

    client.send_numeric(RPL_ENDOFWHO, &[mask, "End of WHO list"]);
}

// ---------------------------------------------------------------------------
// WHOIS — query user info
// ---------------------------------------------------------------------------

pub async fn handle_whois(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
            // RPL_WHOISSPECIAL (320) — NickServ reputation if registered.
            if let Some(svc) = state.services() {
                if let Some(identity) = svc.nickserv.get_identity(&target.info.nick).await {
                    client.send_numeric(
                        RPL_WHOISSPECIAL,
                        &[
                            &target.info.nick,
                            &format!("reputation {}", identity.reputation),
                        ],
                    );
                }
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
// AWAY — set or clear away status (RFC 2812 §4.1)
// ---------------------------------------------------------------------------

pub async fn handle_away(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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

pub async fn handle_ison(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
