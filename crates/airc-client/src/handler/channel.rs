//! Channel membership and nick-change handlers.
//!
//! Covers: JOIN, PART, QUIT, KICK, NICK, TOPIC, AWAY (away-notify),
//! ACCOUNT (account-notify).

use airc_shared::IrcMessage;

use crate::event::IrcEvent;

use super::{ConnContext, extract_nick};

/// Handle a JOIN message.
///
/// Gracefully handles the extended-join format (IRCv3 extended-join cap):
/// `:nick!user@host JOIN #chan account :Real Name`
/// Standard JOIN has only `[0]=#chan`; extended adds `[1]=account` and `[2]=realname`.
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

/// Handle an AWAY broadcast (IRCv3 away-notify).
///
/// `:nick!user@host AWAY [:reason]`
///
/// `message == None` means the user returned from away; `Some(text)` means
/// they set an away reason.
pub async fn handle_away(msg: &IrcMessage, ctx: &ConnContext) {
    let nick = extract_nick(&msg.prefix);
    // params[0] is the optional away message; absent means "back".
    let message = msg.params.first().cloned();
    let _ = ctx.event_tx.send(IrcEvent::Away { nick, message }).await;
}

/// Handle an ACCOUNT notification (IRCv3 account-notify).
///
/// `:nick!user@host ACCOUNT <account|*>`
///
/// `"*"` means the user logged out; any other value is the new account name.
pub async fn handle_account(msg: &IrcMessage, ctx: &ConnContext) {
    let nick = extract_nick(&msg.prefix);
    let raw = msg.params.first().cloned().unwrap_or_default();
    let account = if raw == "*" { None } else { Some(raw) };
    let _ = ctx
        .event_tx
        .send(IrcEvent::AccountNotify { nick, account })
        .await;
}
