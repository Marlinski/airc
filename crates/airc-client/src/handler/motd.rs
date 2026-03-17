//! MOTD numeric handlers (375, 372, 376).

use airc_shared::IrcMessage;

use crate::event::IrcEvent;

use super::ConnContext;

/// 375 RPL_MOTDSTART — beginning of MOTD (no action needed).
pub async fn handle_motd_start(_msg: &IrcMessage, _ctx: &ConnContext) {}

/// 372 RPL_MOTD — one line of the MOTD.
pub async fn handle_motd_line(msg: &IrcMessage, ctx: &ConnContext) {
    let line = msg.params.last().cloned().unwrap_or_default();
    let line = line.strip_prefix("- ").unwrap_or(&line).to_string();
    let _ = ctx.event_tx.send(IrcEvent::Motd { line }).await;
}

/// 376 RPL_ENDOFMOTD — MOTD transmission complete.
pub async fn handle_motd_end(_msg: &IrcMessage, ctx: &ConnContext) {
    let _ = ctx.event_tx.send(IrcEvent::MotdEnd).await;
}
