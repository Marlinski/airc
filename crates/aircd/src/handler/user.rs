//! User-level commands: QUIT, PING, WHO, WHOIS, AWAY, ISON.

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};
use tracing::debug;

use crate::client::{ClientId, cap};
use crate::relay::RelayEvent;
use crate::state::SharedState;

use super::is_channel_name;

// ---------------------------------------------------------------------------
// QUIT — disconnect
// ---------------------------------------------------------------------------

pub async fn handle_quit(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    let reason = msg.params.first().map(|s| s.as_str()).unwrap_or("Quit");
    let client = state.get_client(client_id).await;

    // Notify all peers in shared channels.
    let peers = state.peers_in_shared_channels(client_id).await;
    if let Some(ref client) = client {
        let quit_msg = IrcMessage::quit(Some(reason)).with_prefix(client.prefix());
        for peer in &peers {
            peer.send_message_tagged(&quit_msg);
        }

        // Relay QUIT to remote nodes (they derive nick removal from this).
        state
            .relay_publish(RelayEvent::Quit {
                client_id,
                reason: Some(reason.to_string()),
            })
            .await;
    }

    // Send ERROR to the quitting client.
    if let Some(ref client) = client {
        let error_msg = IrcMessage {
            tags: vec![],
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
        // Use channel_members_with_mode to get member handles and their mode
        // flags in one pass, avoiding a full Channel::clone().
        let multi = client.info.has_cap(cap::MULTI_PREFIX);
        if let Some(members_with_mode) = state.channel_members_with_mode(mask).await {
            for (member, mode) in &members_with_mode {
                // H = here, G = gone (away).
                let away_flag = if member.info.away.is_some() { "G" } else { "H" };
                // Membership prefix: @ for op, + for voiced.
                // With multi-prefix, use multi_prefix() to get all symbols.
                let prefix = if multi {
                    mode.multi_prefix()
                } else if mode.is_op() {
                    "@"
                } else if mode.is_voice() {
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
        //
        // `who_matching_clients` builds the co-member set and applies the
        // visibility + mask filter in a single two-pass iteration, avoiding
        // the O(N) full-clone that `all_clients()` + `co_members()` would
        // require.
        let matching = state.who_matching_clients(client_id, mask).await;
        for target in &matching {
            let away_flag = if target.info.away.is_some() { "G" } else { "H" };
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
            if let Some(ref away_msg) = target.info.away {
                client.send_numeric(RPL_AWAY, &[&target.info.nick, away_msg]);
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
            if let Some(svc) = state.services()
                && let Some(identity) = svc.nickserv.get_identity(&target.info.nick).await {
                    client.send_numeric(
                        RPL_WHOISSPECIAL,
                        &[
                            &target.info.nick,
                            &format!("reputation {}", identity.reputation),
                        ],
                    );
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

    let new_away: Option<String> = if let Some(away_text) = msg.params.first()
        && !away_text.is_empty()
    {
        Some(away_text.clone())
    } else {
        None
    };

    // Update away status in shared state.
    state.set_away(client_id, new_away.clone()).await;

    // Respond to the client.
    if new_away.is_some() {
        client.send_numeric(RPL_NOWAWAY, &["You have been marked as being away"]);
    } else {
        client.send_numeric(RPL_UNAWAY, &["You are no longer marked as being away"]);
    }

    // Broadcast AWAY change to channel members with away-notify capability.
    let away_notify_msg = IrcMessage {
        tags: vec![],
        prefix: Some(client.prefix()),
        command: Command::Away,
        params: match &new_away {
            Some(msg) => vec![msg.clone()],
            None => vec![],
        },
    };

    let peers = state.peers_in_shared_channels(client_id).await;
    for peer in &peers {
        if peer.info.has_cap(cap::AWAY_NOTIFY) {
            peer.send_message_tagged(&away_notify_msg);
        }
    }
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
