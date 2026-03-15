//! NickServ reputation module — VOUCH, REPORT, REPUTATION.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tracing::info;

use crate::services::module::{CommandContext, ServiceModule};
use crate::services::nickserv::NickServState;

/// Reputation management module for NickServ.
pub struct ReputationModule {
    state: Arc<NickServState>,
}

impl ReputationModule {
    pub fn new(state: Arc<NickServState>) -> Self {
        Self { state }
    }

    // -- VOUCH <nick> -------------------------------------------------------

    async fn cmd_vouch(&self, ctx: &CommandContext<'_>) {
        let Some(target) = ctx.arg1 else {
            ctx.reply("Usage: VOUCH <nick>").await;
            return;
        };

        let sender_lower = ctx.sender.to_ascii_lowercase();

        // Sender must be registered.
        if self.state.get_identity(ctx.sender).await.is_none() {
            ctx.reply("You must be registered to vouch for someone.")
                .await;
            return;
        }

        // Cannot vouch for yourself.
        if target.to_ascii_lowercase() == sender_lower {
            ctx.reply("You cannot vouch for yourself.").await;
            return;
        }

        // Rate-limit: one vouch per (sender, target) every 5 minutes.
        if let Err(remaining) = self
            .state
            .check_rate_limit(ctx.sender, "vouch", target)
            .await
        {
            ctx.reply(&format!("Rate limited. Try again in {remaining} seconds."))
                .await;
            return;
        }

        match self.state.modify_reputation(target, 1).await {
            Some(new_score) => {
                ctx.reply(&format!(
                    "You vouched for \x02{target}\x02. Their reputation is now {new_score}."
                ))
                .await;
                info!(sender = %ctx.sender, target = %target, "NickServ: vouch");
            }
            None => {
                ctx.reply(&format!("{target} is not registered.")).await;
            }
        }
    }

    // -- REPORT <nick> ------------------------------------------------------

    async fn cmd_report(&self, ctx: &CommandContext<'_>) {
        let Some(target) = ctx.arg1 else {
            ctx.reply("Usage: REPORT <nick>").await;
            return;
        };

        let sender_lower = ctx.sender.to_ascii_lowercase();

        // Sender must be registered.
        if self.state.get_identity(ctx.sender).await.is_none() {
            ctx.reply("You must be registered to report someone.").await;
            return;
        }

        // Cannot report yourself.
        if target.to_ascii_lowercase() == sender_lower {
            ctx.reply("You cannot report yourself.").await;
            return;
        }

        // Rate-limit: one report per (sender, target) every 5 minutes.
        if let Err(remaining) = self
            .state
            .check_rate_limit(ctx.sender, "report", target)
            .await
        {
            ctx.reply(&format!("Rate limited. Try again in {remaining} seconds."))
                .await;
            return;
        }

        match self.state.modify_reputation(target, -1).await {
            Some(new_score) => {
                ctx.reply(&format!(
                    "You reported \x02{target}\x02. Their reputation is now {new_score}."
                ))
                .await;
                info!(sender = %ctx.sender, target = %target, "NickServ: report");
            }
            None => {
                ctx.reply(&format!("{target} is not registered.")).await;
            }
        }
    }

    // -- REPUTATION <nick> --------------------------------------------------

    async fn cmd_reputation(&self, ctx: &CommandContext<'_>) {
        let Some(nick) = ctx.arg1 else {
            ctx.reply("Usage: REPUTATION <nick>").await;
            return;
        };

        match self.state.get_identity(nick).await {
            Some(identity) => {
                ctx.reply(&format!(
                    "Reputation for \x02{}\x02: {}",
                    identity.nick, identity.reputation
                ))
                .await;
            }
            None => {
                ctx.reply(&format!("{nick} is not registered.")).await;
            }
        }
    }
}

impl ServiceModule for ReputationModule {
    fn name(&self) -> &str {
        "reputation"
    }

    fn commands(&self) -> &[&str] {
        &["VOUCH", "REPORT", "REPUTATION"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match ctx.command {
                "VOUCH" => self.cmd_vouch(ctx).await,
                "REPORT" => self.cmd_report(ctx).await,
                "REPUTATION" => self.cmd_reputation(ctx).await,
                _ => return false,
            }
            true
        })
    }

    fn help_lines(&self) -> Vec<String> {
        vec![
            "  VOUCH <nick>              \u{2014} Give +1 reputation to a nick".into(),
            "  REPORT <nick>             \u{2014} Give -1 reputation to a nick".into(),
            "  REPUTATION <nick>         \u{2014} Query a nick's reputation score".into(),
        ]
    }
}
