//! PRIVMSG and NOTICE — message routing (channel fan-out and DMs).

use std::sync::Arc;

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};

use crate::client::ClientId;
use crate::relay::RelayEvent;
use crate::state::{ChannelSendResult, SharedState};

use super::is_channel_name;

pub async fn handle_privmsg(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    route_message(state, client_id, msg, Command::Privmsg).await;
}

pub async fn handle_notice(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
        // Single lock acquisition: checks +n, +m, and collects fan-out targets.
        match state.check_channel_send(target, client_id).await {
            ChannelSendResult::NoSuchChannel => {
                if cmd == Command::Privmsg {
                    client.send_numeric(ERR_NOSUCHCHANNEL, &[target, "No such channel"]);
                }
            }
            ChannelSendResult::NoExternal => {
                if cmd == Command::Privmsg {
                    client.send_numeric(ERR_CANNOTSENDTOCHAN, &[target, "Cannot send to channel"]);
                }
            }
            ChannelSendResult::Moderated => {
                if cmd == Command::Privmsg {
                    client.send_numeric(ERR_CANNOTSENDTOCHAN, &[target, "Cannot send to channel"]);
                }
            }
            ChannelSendResult::Ok(members) => {
                let line: Arc<str> = outgoing.serialize().into();
                for member in &members {
                    member.send_line(&line);
                }
                // Relay to remote nodes.
                if cmd == Command::Notice {
                    state
                        .relay_publish(RelayEvent::Notice {
                            client_id,
                            target: target.to_string(),
                            text: text.to_string(),
                        })
                        .await;
                } else {
                    state
                        .relay_publish(RelayEvent::Privmsg {
                            client_id,
                            target: target.to_string(),
                            text: text.to_string(),
                        })
                        .await;
                }
            }
        }
    } else {
        // Direct message to a user.

        // Service dispatch — intercept PRIVMSG to NickServ / ChanServ.
        if cmd == Command::Privmsg {
            let target_lower = target.to_ascii_lowercase();
            if target_lower == "nickserv" {
                match state.services() {
                    Some(svc) => {
                        svc.dispatch_nickserv(&client.info.nick, text, &client)
                            .await
                    }
                    None => {
                        client.send_numeric(ERR_NOSUCHNICK, &[target, "NickServ is not available"])
                    }
                }
                return;
            }
            if target_lower == "chanserv" {
                match state.services() {
                    Some(svc) => {
                        svc.dispatch_chanserv(&client.info.nick, text, &client)
                            .await
                    }
                    None => {
                        client.send_numeric(ERR_NOSUCHNICK, &[target, "ChanServ is not available"])
                    }
                }
                return;
            }
        }

        // Look up target (local or remote) and route accordingly.
        match state.find_user_by_nick(target).await {
            Some(target_client) if target_client.is_local() => {
                target_client.send_message(&outgoing);

                // RPL_AWAY to sender if target is away.
                if cmd == Command::Privmsg
                    && let Some(ref away_msg) = target_client.info.away
                {
                    client.send_numeric(RPL_AWAY, &[&target_client.info.nick, away_msg]);
                }
            }
            Some(_remote_client) => {
                // Target is on a remote node — relay to it.
                if cmd == Command::Notice {
                    state
                        .relay_publish(RelayEvent::Notice {
                            client_id,
                            target: target.to_string(),
                            text: text.to_string(),
                        })
                        .await;
                } else {
                    state
                        .relay_publish(RelayEvent::Privmsg {
                            client_id,
                            target: target.to_string(),
                            text: text.to_string(),
                        })
                        .await;
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
