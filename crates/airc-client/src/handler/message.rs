//! PRIVMSG and NOTICE handlers, including CTCP ACTION detection.

use airc_shared::IrcMessage;
use airc_shared::validate::is_channel_name;

use crate::event::{IrcEvent, MessageKind, new_channel_message};

use super::{ConnContext, extract_nick};

/// Handle a PRIVMSG.
pub async fn handle_privmsg(msg: &IrcMessage, ctx: &ConnContext) {
    if msg.params.len() < 2 {
        return;
    }
    let target = &msg.params[0];
    let text = &msg.params[1];
    let from = extract_nick(&msg.prefix);

    let (text, kind) = if text.starts_with("\x01ACTION ") && text.ends_with('\x01') {
        let inner = &text[8..text.len() - 1];
        (inner.to_string(), MessageKind::Action)
    } else {
        (text.clone(), MessageKind::Normal)
    };

    let cm = new_channel_message(target.clone(), from.clone(), text.clone(), kind);

    if is_channel_name(target) {
        ctx.state.push_message(target, cm).await;
    } else {
        ctx.state.push_private_message(cm).await;
    }

    let _ = ctx
        .event_tx
        .send(IrcEvent::Message(new_channel_message(
            target.clone(),
            from,
            text,
            MessageKind::Normal,
        )))
        .await;
}

/// Handle a NOTICE.
pub async fn handle_notice(msg: &IrcMessage, ctx: &ConnContext) {
    let target = msg.params.first().cloned().unwrap_or_default();
    let text = msg.params.get(1).cloned().unwrap_or_default();
    let from = msg.prefix.as_ref().map(|p| extract_nick(&Some(p.clone())));

    if let Some(ref from_nick) = from {
        let cm = new_channel_message(
            target.clone(),
            from_nick.clone(),
            text.clone(),
            MessageKind::Normal,
        );
        if is_channel_name(&target) {
            ctx.state.push_message(&target, cm).await;
        } else {
            ctx.state.push_private_message(cm).await;
        }
    }

    let _ = ctx
        .event_tx
        .send(IrcEvent::Notice { from, target, text })
        .await;
}
