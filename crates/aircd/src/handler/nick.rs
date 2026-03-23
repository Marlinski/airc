//! NICK — change nickname after registration.

use airc_shared::IrcMessage;
use airc_shared::reply::*;
use tracing::debug;

use crate::client::ClientId;
use crate::relay::RelayEvent;
use crate::state::SharedState;

pub async fn handle_nick(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
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
            client.send_message_tagged(&nick_msg);

            let peers = state.peers_in_shared_channels(client_id).await;
            for peer in &peers {
                peer.send_message_tagged(&nick_msg);
            }

            // Relay nick change to remote nodes.
            state
                .relay_publish(RelayEvent::NickChange {
                    client_id,
                    new_nick: new_nick.to_string(),
                })
                .await;

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
