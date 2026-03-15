//! Server-level commands: MOTD, VERSION, LUSERS, OPER, KILL.

use std::sync::Arc;

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};
use tracing::debug;

use crate::client::ClientId;
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// MOTD — message of the day
// ---------------------------------------------------------------------------

pub async fn handle_motd(state: &SharedState, client_id: ClientId) {
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

pub async fn handle_oper(state: &SharedState, client_id: ClientId, msg: &airc_shared::IrcMessage) {
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

pub async fn handle_kill(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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

pub async fn handle_version(state: &SharedState, client_id: ClientId) {
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

pub async fn handle_lusers(state: &SharedState, client_id: ClientId) {
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
