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

    // NickServ IDENTIFY fallback: if we have a password but SASL did not
    // complete (e.g., server didn't advertise SASL, or SASL failed), send
    // a NickServ IDENTIFY as a best-effort fallback.
    if let Some(ref pw) = ctx.password
        && !ctx.state.is_sasl_logged_in().await {
            let _ = ctx
                .line_tx
                .send(IrcMessage::privmsg("NickServ", &format!("IDENTIFY {pw}")).serialize())
                .await;
        }

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
        // Strip all leading prefix chars.  With `multi-prefix` a nick may
        // carry multiple prefixes (e.g. `@+nick`).  `trim_start_matches` with
        // a char slice strips all leading chars in the set, so this handles
        // both the single-prefix and multi-prefix cases correctly.
        let members: Vec<String> = names_str
            .split_whitespace()
            .map(|n| {
                n.trim_start_matches(['@', '+', '%', '~', '&'])
                    .to_string()
            })
            .collect();
        ctx.state.set_members(channel, members).await;
    }
}
