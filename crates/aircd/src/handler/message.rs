//! PRIVMSG and NOTICE — message routing (channel fan-out and DMs).

use std::sync::Arc;

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};

use crate::client::{ClientId, ClientKind};
use crate::state::SharedState;

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

        // Fan out to all channel members except the sender.
        match state.channel_members_except(target, client_id).await {
            Some(members) => {
                let line: Arc<str> = outgoing.serialize().into();
                for member in &members {
                    member.send_line(&line);
                }
                state.relay_publish(&outgoing).await;
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

        // Service dispatch — intercept PRIVMSG to NickServ / ChanServ.
        if cmd == Command::Privmsg {
            let target_lower = target.to_ascii_lowercase();
            if target_lower == "nickserv" {
                match state.services() {
                    Some(svc) => svc.dispatch_nickserv(&client.info.nick, text, &client).await,
                    None => client
                        .send_numeric(ERR_NOSUCHNICK, &[target, "NickServ is not available"]),
                }
                return;
            }
            if target_lower == "chanserv" {
                match state.services() {
                    Some(svc) => svc.dispatch_chanserv(&client.info.nick, text, &client).await,
                    None => client
                        .send_numeric(ERR_NOSUCHNICK, &[target, "ChanServ is not available"]),
                }
                return;
            }
        }

        // Check nick_kind to handle both local and remote routing.
        match state.nick_kind(target).await {
            Some(ClientKind::Local(_)) => {
                if let Some(target_client) = state.find_client_by_nick(target).await {
                    target_client.send_message(&outgoing);

                    // RPL_AWAY to sender if target is away.
                    if cmd == Command::Privmsg
                        && let Some(away_msg) = state.get_away_message(target_client.id).await
                    {
                        client.send_numeric(RPL_AWAY, &[&target_client.info.nick, &away_msg]);
                    }

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
                state.relay_publish(&outgoing).await;
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
