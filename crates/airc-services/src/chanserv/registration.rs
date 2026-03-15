//! ChanServ registration module — REGISTER, INFO, SET.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tracing::info;

use crate::chanserv::{ChanServState, RegisteredChannel};
use crate::module::{CommandContext, ServiceModule};

/// Channel registration and settings module for ChanServ.
pub struct RegistrationModule {
    state: Arc<ChanServState>,
}

impl RegistrationModule {
    pub fn new(state: Arc<ChanServState>) -> Self {
        Self { state }
    }

    // -- REGISTER <channel> [description] -----------------------------------

    async fn cmd_register(&self, ctx: &CommandContext<'_>) {
        let Some(channel) = ctx.arg1 else {
            ctx.reply("Usage: REGISTER <#channel> [description]").await;
            return;
        };

        if !channel.starts_with('#') && !channel.starts_with('&') {
            ctx.reply("Invalid channel name.").await;
            return;
        }

        let reg = RegisteredChannel {
            name: channel.to_string(),
            founder: ctx.sender.to_ascii_lowercase(),
            min_reputation: 0,
            bans: Vec::new(),
            description: ctx.arg2.map(|s| s.to_string()),
        };

        if !self.state.register_channel(reg).await {
            ctx.reply("This channel is already registered.").await;
            return;
        }

        ctx.reply(&format!(
            "Channel {channel} registered. You are the founder."
        ))
        .await;
        info!(channel = %channel, founder = %ctx.sender, "ChanServ: channel registered");
    }

    // -- INFO <channel> -----------------------------------------------------

    async fn cmd_info(&self, ctx: &CommandContext<'_>) {
        let Some(channel) = ctx.arg1 else {
            ctx.reply("Usage: INFO <#channel>").await;
            return;
        };

        match self.state.get_channel(channel).await {
            None => {
                ctx.reply(&format!("{channel} is not registered.")).await;
            }
            Some(reg) => {
                ctx.reply(&format!("Information for \x02{}\x02:", reg.name))
                    .await;
                ctx.reply(&format!("  Founder:         {}", reg.founder))
                    .await;
                ctx.reply(&format!("  Min reputation:  {}", reg.min_reputation))
                    .await;
                ctx.reply(&format!("  Bans:            {}", reg.bans.len()))
                    .await;
                if let Some(ref desc) = reg.description {
                    ctx.reply(&format!("  Description:     {desc}")).await;
                }
            }
        }
    }

    // -- SET <channel> <key> <value> ----------------------------------------

    async fn cmd_set(&self, ctx: &CommandContext<'_>) {
        let Some(channel) = ctx.arg1 else {
            ctx.reply("Usage: SET <#channel> <key> <value>").await;
            return;
        };

        // Parse key and value from the rest (arg2 = "<key> <value>").
        let (key, value) = match ctx.arg2 {
            Some(rest) => {
                let mut kv = rest.splitn(2, ' ');
                (kv.next(), kv.next())
            }
            None => (None, None),
        };

        let Some(key) = key else {
            ctx.reply("Available settings: MIN-REPUTATION, DESCRIPTION")
                .await;
            return;
        };

        // Only founder can modify.
        if !self.state.is_founder(channel, ctx.sender).await {
            ctx.reply("You are not the founder of this channel.").await;
            return;
        }

        match key.to_ascii_uppercase().as_str() {
            "MIN-REPUTATION" | "MINREP" => {
                let Some(val) = value.and_then(|v| v.parse::<i64>().ok()) else {
                    ctx.reply("Usage: SET <#channel> MIN-REPUTATION <number>")
                        .await;
                    return;
                };
                if !self
                    .state
                    .modify_channel(channel, |reg| {
                        reg.min_reputation = val;
                    })
                    .await
                {
                    ctx.reply("This channel is not registered.").await;
                    return;
                }
                ctx.reply(&format!("Minimum reputation for {channel} set to {val}."))
                    .await;
            }
            "DESCRIPTION" | "DESC" => {
                let desc = value.map(|s| s.to_string());
                if !self
                    .state
                    .modify_channel(channel, |reg| {
                        reg.description = desc;
                    })
                    .await
                {
                    ctx.reply("This channel is not registered.").await;
                    return;
                }
                ctx.reply("Channel description updated.").await;
            }
            _ => {
                ctx.reply("Unknown setting. Available: MIN-REPUTATION, DESCRIPTION")
                    .await;
            }
        }
    }
}

impl ServiceModule for RegistrationModule {
    fn name(&self) -> &str {
        "registration"
    }

    fn commands(&self) -> &[&str] {
        &["REGISTER", "INFO", "SET"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match ctx.command {
                "REGISTER" => self.cmd_register(ctx).await,
                "INFO" => self.cmd_info(ctx).await,
                "SET" => self.cmd_set(ctx).await,
                _ => return false,
            }
            true
        })
    }

    fn help_lines(&self) -> Vec<String> {
        vec![
            "  REGISTER <#channel> [desc]     \u{2014} Register a channel (you become founder)"
                .into(),
            "  INFO <#channel>                \u{2014} View channel registration info".into(),
            "  SET <#channel> <key> <value>   \u{2014} Change channel settings".into(),
            "    Settings: MIN-REPUTATION, DESCRIPTION".into(),
        ]
    }
}
