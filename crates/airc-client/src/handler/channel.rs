//! Channel membership and nick-change handlers.
//!
//! Covers: JOIN, PART, QUIT, KICK, NICK, TOPIC.

use airc_shared::IrcMessage;

use crate::event::IrcEvent;

use super::{ConnContext, extract_nick};

/// Handle a JOIN message.
pub async fn handle_join(msg: &IrcMessage, ctx: &ConnContext) {
    let channel = msg.params.first().cloned().unwrap_or_default();
    let nick = extract_nick(&msg.prefix);
    let our_nick = ctx.state.nick().await;

    if nick.eq_ignore_ascii_case(&our_nick) {
        ctx.state.join_channel(&channel).await;
    } else {
        ctx.state.add_member(&channel, &nick).await;
    }
    let _ = ctx.event_tx.send(IrcEvent::Join { nick, channel }).await;
}

/// Handle a PART message.
pub async fn handle_part(msg: &IrcMessage, ctx: &ConnContext) {
    let channel = msg.params.first().cloned().unwrap_or_default();
    let reason = msg.params.get(1).cloned();
    let nick = extract_nick(&msg.prefix);
    let our_nick = ctx.state.nick().await;

    if nick.eq_ignore_ascii_case(&our_nick) {
        ctx.state.part_channel(&channel).await;
    } else {
        ctx.state.remove_member(&channel, &nick).await;
    }
    let _ = ctx
        .event_tx
        .send(IrcEvent::Part {
            nick,
            channel,
            reason,
        })
        .await;
}

/// Handle a QUIT message.
pub async fn handle_quit(msg: &IrcMessage, ctx: &ConnContext) {
    let nick = extract_nick(&msg.prefix);
    let reason = msg.params.first().cloned();
    ctx.state.remove_member_all(&nick).await;
    let _ = ctx.event_tx.send(IrcEvent::Quit { nick, reason }).await;
}

/// Handle a KICK message.
pub async fn handle_kick(msg: &IrcMessage, ctx: &ConnContext) {
    if msg.params.len() >= 2 {
        let channel = msg.params[0].clone();
        let kicked = msg.params[1].clone();
        let reason = msg.params.get(2).cloned();
        let by = extract_nick(&msg.prefix);
        let our_nick = ctx.state.nick().await;

        if kicked.eq_ignore_ascii_case(&our_nick) {
            ctx.state.part_channel(&channel).await;
        } else {
            ctx.state.remove_member(&channel, &kicked).await;
        }
        let _ = ctx
            .event_tx
            .send(IrcEvent::Kick {
                channel,
                nick: kicked,
                by,
                reason,
            })
            .await;
    }
}

/// Handle a NICK change.
pub async fn handle_nick(msg: &IrcMessage, ctx: &ConnContext) {
    let old_nick = extract_nick(&msg.prefix);
    let new_nick = msg.params.first().cloned().unwrap_or_default();
    let our_nick = ctx.state.nick().await;

    if old_nick.eq_ignore_ascii_case(&our_nick) {
        ctx.state.set_nick(new_nick.clone()).await;
    }
    ctx.state.rename_member(&old_nick, &new_nick).await;
    let _ = ctx
        .event_tx
        .send(IrcEvent::NickChange { old_nick, new_nick })
        .await;
}

/// Handle a TOPIC change.
pub async fn handle_topic(msg: &IrcMessage, ctx: &ConnContext) {
    let channel = msg.params.first().cloned().unwrap_or_default();
    let topic = msg.params.get(1).cloned().unwrap_or_default();
    let set_by = extract_nick(&msg.prefix);
    ctx.state.set_topic(&channel, topic.clone()).await;
    let _ = ctx
        .event_tx
        .send(IrcEvent::TopicChange {
            channel,
            topic,
            set_by,
        })
        .await;
}
