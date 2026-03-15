//! NickServ silence module — client-side message filtering.
//!
//! SILENCE was historically a server-side IRC command (UnrealIRCd / Bahamut
//! extension).  In AIRC the server no longer enforces silence — filtering is
//! done client-side.  NickServ simply stores and serves the per-identity
//! silence list so clients can rebuild their local filter on reconnect.
//!
//! Commands:
//! - `SILENCE`               — list your silenced nicks
//! - `SILENCE +nick [reason]` — add a nick to your silence list
//! - `SILENCE -nick`          — remove a nick from your silence list
//! - `SILENCE +n1 -n2 +n3`   — batch add/remove (reason not supported in batch)

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tracing::{debug, info};

use crate::module::{CommandContext, ServiceModule};
use crate::nickserv::NickServState;

/// Silence module for NickServ (SILENCE command).
pub struct SilenceModule {
    state: Arc<NickServState>,
}

impl SilenceModule {
    pub fn new(state: Arc<NickServState>) -> Self {
        Self { state }
    }

    async fn cmd_silence(&self, ctx: &CommandContext<'_>) {
        // Sender must be registered.
        if self.state.get_identity(ctx.sender).await.is_none() {
            ctx.reply("You must be registered to use SILENCE.").await;
            return;
        }

        // No args → list silenced nicks.
        if ctx.raw_args.is_empty() {
            let entries = self.state.get_silence_list(ctx.sender).await;
            if entries.is_empty() {
                ctx.reply("Your silence list is empty.").await;
            } else {
                for entry in &entries {
                    if let Some(ref reason) = entry.reason {
                        ctx.reply(&format!("SILENCE +{} {}", entry.nick, reason))
                            .await;
                    } else {
                        ctx.reply(&format!("SILENCE +{}", entry.nick)).await;
                    }
                }
                ctx.reply("End of silence list.").await;
            }
            return;
        }

        // Single token with reason? e.g. "+nick some reason here"
        // Check if the first token starts with + and there are more tokens that
        // do NOT start with +/-, in which case everything after the nick is a reason.
        let tokens: Vec<&str> = ctx.raw_args.split_whitespace().collect();

        let is_single_add_with_reason = tokens.len() >= 2
            && tokens[0].starts_with('+')
            && !tokens[1].starts_with('+')
            && !tokens[1].starts_with('-');

        if is_single_add_with_reason {
            let nick = &tokens[0][1..]; // strip '+'
            let reason = tokens[1..].join(" ");
            self.add_silence(ctx, nick, Some(&reason)).await;
            return;
        }

        // Batch mode: +nick / -nick tokens.
        for token in &tokens {
            let (adding, target_nick) = if let Some(nick) = token.strip_prefix('+') {
                (true, nick)
            } else if let Some(nick) = token.strip_prefix('-') {
                (false, nick)
            } else {
                // Bare nick treated as +nick (add to silence list).
                (true, *token)
            };

            if target_nick.is_empty() {
                ctx.reply("Usage: SILENCE [+nick|-nick ...]").await;
                continue;
            }

            if adding {
                self.add_silence(ctx, target_nick, None).await;
            } else {
                self.remove_silence(ctx, target_nick).await;
            }
        }

        info!(sender = %ctx.sender, "NickServ: SILENCE command processed");
    }

    async fn add_silence(&self, ctx: &CommandContext<'_>, nick: &str, reason: Option<&str>) {
        // Cannot silence yourself.
        if nick.eq_ignore_ascii_case(ctx.sender) {
            ctx.reply("You cannot silence yourself.").await;
            return;
        }

        if self.state.add_silence(ctx.sender, nick, reason).await {
            ctx.reply(&format!("\x02{nick}\x02 has been silenced."))
                .await;
            debug!(sender = %ctx.sender, target = %nick, "NickServ: silence +nick");
        } else {
            ctx.reply(&format!("\x02{nick}\x02 is already in your silence list."))
                .await;
        }
    }

    async fn remove_silence(&self, ctx: &CommandContext<'_>, nick: &str) {
        if self.state.remove_silence(ctx.sender, nick).await {
            ctx.reply(&format!("\x02{nick}\x02 has been unsilenced."))
                .await;
            debug!(sender = %ctx.sender, target = %nick, "NickServ: silence -nick");
        } else {
            ctx.reply(&format!("\x02{nick}\x02 is not in your silence list."))
                .await;
        }
    }
}

impl ServiceModule for SilenceModule {
    fn name(&self) -> &str {
        "silence"
    }

    fn commands(&self) -> &[&str] {
        &["SILENCE"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match ctx.command {
                "SILENCE" => self.cmd_silence(ctx).await,
                _ => return false,
            }
            true
        })
    }

    fn help_lines(&self) -> Vec<String> {
        vec![
            "  SILENCE                   \u{2014} List silenced nicks".into(),
            "  SILENCE +nick [reason]    \u{2014} Silence a nick".into(),
            "  SILENCE -nick             \u{2014} Unsilence a nick".into(),
            "  SILENCE +nick1 -nick2     \u{2014} Batch add/remove".into(),
        ]
    }
}
