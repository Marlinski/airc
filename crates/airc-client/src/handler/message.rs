//! PRIVMSG and NOTICE handlers, including CTCP ACTION detection.

use airc_shared::IrcMessage;
use airc_shared::validate::is_channel_name;

use crate::event::{IrcEvent, MessageKind, new_channel_message_with_ts};

use super::{ConnContext, extract_nick};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the `server-time` from IRCv3 message tags.
///
/// Looks for a `time` tag whose value is an ISO 8601 / RFC 3339 timestamp
/// (e.g. `"2023-01-01T12:34:56.789Z"`).  Returns `None` if the tag is absent
/// or cannot be parsed.
fn extract_server_time(msg: &IrcMessage) -> Option<u64> {
    msg.tags
        .iter()
        .find(|(k, _)| k == "time")
        .and_then(|(_, v)| v.as_ref())
        .and_then(|t| {
            chrono::DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|dt| dt.timestamp().max(0) as u64)
        })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

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

    let server_ts = extract_server_time(msg);

    let cm =
        new_channel_message_with_ts(target.clone(), from.clone(), text.clone(), kind, server_ts);

    if is_channel_name(target) {
        ctx.state.push_message(target, cm).await;
    } else {
        ctx.state.push_private_message(cm).await;
    }

    let _ = ctx
        .event_tx
        .send(IrcEvent::Message(new_channel_message_with_ts(
            target.clone(),
            from,
            text,
            MessageKind::Normal,
            server_ts,
        )))
        .await;
}

/// Handle a NOTICE.
pub async fn handle_notice(msg: &IrcMessage, ctx: &ConnContext) {
    let target = msg.params.first().cloned().unwrap_or_default();
    let text = msg.params.get(1).cloned().unwrap_or_default();
    let from = msg.prefix.as_ref().map(|p| extract_nick(&Some(p.clone())));

    let server_ts = extract_server_time(msg);

    if let Some(ref from_nick) = from {
        let cm = new_channel_message_with_ts(
            target.clone(),
            from_nick.clone(),
            text.clone(),
            MessageKind::Normal,
            server_ts,
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
