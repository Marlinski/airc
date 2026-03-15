//! ChanServ access module — BAN, UNBAN.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::services::chanserv::ChanServState;
use crate::services::module::{CommandContext, ServiceModule};

/// Access control module for ChanServ (bans).
pub struct AccessModule {
    state: Arc<ChanServState>,
}

impl AccessModule {
    pub fn new(state: Arc<ChanServState>) -> Self {
        Self { state }
    }

    // -- BAN/UNBAN <channel> <nick-pattern> ---------------------------------

    async fn cmd_ban(&self, ctx: &CommandContext<'_>, add: bool) {
        let cmd_name = if add { "BAN" } else { "UNBAN" };

        let Some(channel) = ctx.arg1 else {
            ctx.reply(&format!("Usage: {cmd_name} <#channel> <nick-pattern>"))
                .await;
            return;
        };
        let Some(pattern) = ctx.arg2 else {
            ctx.reply(&format!("Usage: {cmd_name} <#channel> <nick-pattern>"))
                .await;
            return;
        };

        if !self.state.is_founder(channel, ctx.sender).await {
            ctx.reply("You are not the founder of this channel.").await;
            return;
        }

        let pattern_lower = pattern.to_ascii_lowercase();

        if add {
            if !self
                .state
                .modify_channel(channel, |reg| {
                    if !reg.bans.contains(&pattern_lower) {
                        reg.bans.push(pattern_lower.clone());
                    }
                })
                .await
            {
                ctx.reply("This channel is not registered.").await;
                return;
            }
            ctx.reply(&format!("Banned \x02{pattern}\x02 from {channel}."))
                .await;
        } else {
            if !self
                .state
                .modify_channel(channel, |reg| {
                    reg.bans.retain(|b| *b != pattern_lower);
                })
                .await
            {
                ctx.reply("This channel is not registered.").await;
                return;
            }
            ctx.reply(&format!("Unbanned \x02{pattern}\x02 from {channel}."))
                .await;
        }
    }
}

impl ServiceModule for AccessModule {
    fn name(&self) -> &str {
        "access"
    }

    fn commands(&self) -> &[&str] {
        &["BAN", "UNBAN"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match ctx.command {
                "BAN" => self.cmd_ban(ctx, true).await,
                "UNBAN" => self.cmd_ban(ctx, false).await,
                _ => return false,
            }
            true
        })
    }

    fn help_lines(&self) -> Vec<String> {
        vec![
            "  BAN <#channel> <nick-pattern>  \u{2014} Ban a nick pattern from joining".into(),
            "  UNBAN <#channel> <nick-pattern> \u{2014} Remove a ban".into(),
        ]
    }
}
