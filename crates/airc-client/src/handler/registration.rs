//! Registration-phase numeric handlers.
//!
//! Covers:
//! - 001 RPL_WELCOME  — marks the client as registered
//! - 433 ERR_NICKNAMEINUSE — appends `_` and retries
//! - 332 RPL_TOPIC    — stores the channel topic from the server burst
//! - 353 RPL_NAMREPLY — stores the initial member list

use tracing::warn;

use airc_shared::{IrcMessage, IrcMessage as M};

use crate::event::IrcEvent;

use super::ConnContext;

/// 001 RPL_WELCOME — registration complete.
pub async fn handle_welcome(msg: &IrcMessage, ctx: &ConnContext) {
    let nick = msg.params.first().cloned().unwrap_or_default();
    let server = msg.prefix.clone().unwrap_or_default();
    let message = msg.params.last().cloned().unwrap_or_default();
    ctx.state.set_nick(nick.clone()).await;
    ctx.state.set_server_name(server.clone()).await;
    ctx.state.set_registered().await;
    let _ = ctx
        .event_tx
        .send(IrcEvent::Registered {
            nick,
            server,
            message,
        })
        .await;
}

/// 433 ERR_NICKNAMEINUSE — append `_` and retry.
pub async fn handle_nick_in_use(_msg: &IrcMessage, ctx: &ConnContext) {
    let current = ctx.state.nick().await;
    let new_nick = format!("{current}_");
    ctx.state.set_nick(new_nick.clone()).await;
    let _ = ctx.line_tx.send(M::nick(&new_nick).serialize()).await;
    warn!(nick = %new_nick, "nick in use, trying alternative");
}

/// 332 RPL_TOPIC — server-burst topic for a channel.
pub async fn handle_topic_reply(msg: &IrcMessage, ctx: &ConnContext) {
    if msg.params.len() >= 3 {
        let channel = &msg.params[1];
        let topic = &msg.params[2];
        ctx.state.set_topic(channel, topic.clone()).await;
    }
}

/// 353 RPL_NAMREPLY — initial member list for a channel.
pub async fn handle_names_reply(msg: &IrcMessage, ctx: &ConnContext) {
    if msg.params.len() >= 4 {
        let channel = &msg.params[2];
        let names_str = &msg.params[3];
        let members: Vec<String> = names_str
            .split_whitespace()
            .map(|n| {
                n.trim_start_matches(|c| c == '@' || c == '+' || c == '%')
                    .to_string()
            })
            .collect();
        ctx.state.set_members(channel, members).await;
    }
}
