//! NickServ social module — FRIEND.
//!
//! This module manages per-identity friend lists. Unlike the former aircd
//! implementation (which used ephemeral ClientId-based lists), friends here
//! are stored by registered nick and persisted to disk.
//!
//! Commands:
//! - `FRIEND` — list your friends
//! - `FRIEND +nick` or `FRIEND nick` — add a friend
//! - `FRIEND -nick` — remove a friend
//! - `FRIEND +nick1 +nick2 -nick3` — batch add/remove

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tracing::{debug, info};

use crate::module::{CommandContext, ServiceModule};
use crate::nickserv::NickServState;

/// Social graph module for NickServ (FRIEND command).
pub struct SocialModule {
    state: Arc<NickServState>,
}

impl SocialModule {
    pub fn new(state: Arc<NickServState>) -> Self {
        Self { state }
    }

    async fn cmd_friend(&self, ctx: &CommandContext<'_>) {
        // Sender must be registered.
        if self.state.get_identity(ctx.sender).await.is_none() {
            ctx.reply("You must be registered to use FRIEND.").await;
            return;
        }

        // No args → list friends.
        if ctx.raw_args.is_empty() {
            let friends = self.state.get_friend_list(ctx.sender).await;
            if friends.is_empty() {
                ctx.reply("Your friend list is empty.").await;
            } else {
                for friend in &friends {
                    ctx.reply(&format!("FRIEND +{friend}")).await;
                }
                ctx.reply("End of friend list.").await;
            }
            return;
        }

        // Parse multi-nick: +nick / -nick / bare nick (treated as +nick).
        let tokens: Vec<&str> = ctx.raw_args.split_whitespace().collect();
        for token in tokens {
            let (adding, target_nick) = if let Some(nick) = token.strip_prefix('+') {
                (true, nick)
            } else if let Some(nick) = token.strip_prefix('-') {
                (false, nick)
            } else {
                // Bare nick treated as +nick (add friend).
                (true, token)
            };

            if target_nick.is_empty() {
                ctx.reply("Usage: FRIEND [+nick|-nick ...]").await;
                continue;
            }

            // Cannot friend yourself.
            if target_nick.eq_ignore_ascii_case(ctx.sender) {
                ctx.reply("You cannot friend yourself.").await;
                continue;
            }

            // Target must be registered.
            if self.state.get_identity(target_nick).await.is_none() {
                ctx.reply(&format!("{target_nick} is not registered."))
                    .await;
                continue;
            }

            if adding {
                if self.state.add_friend(ctx.sender, target_nick).await {
                    ctx.reply(&format!("\x02{target_nick}\x02 is now your friend."))
                        .await;
                    debug!(sender = %ctx.sender, target = %target_nick, "NickServ: friend +nick");
                } else {
                    ctx.reply(&format!(
                        "\x02{target_nick}\x02 is already in your friend list."
                    ))
                    .await;
                }
            } else {
                if self.state.remove_friend(ctx.sender, target_nick).await {
                    ctx.reply(&format!("\x02{target_nick}\x02 is no longer your friend."))
                        .await;
                    debug!(sender = %ctx.sender, target = %target_nick, "NickServ: friend -nick");
                } else {
                    ctx.reply(&format!(
                        "\x02{target_nick}\x02 is not in your friend list."
                    ))
                    .await;
                }
            }
        }

        info!(sender = %ctx.sender, "NickServ: FRIEND command processed");
    }
}

impl ServiceModule for SocialModule {
    fn name(&self) -> &str {
        "social"
    }

    fn commands(&self) -> &[&str] {
        &["FRIEND"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match ctx.command {
                "FRIEND" => self.cmd_friend(ctx).await,
                _ => return false,
            }
            true
        })
    }

    fn help_lines(&self) -> Vec<String> {
        vec![
            "  FRIEND                    \u{2014} List your friends".into(),
            "  FRIEND +nick              \u{2014} Add a friend".into(),
            "  FRIEND -nick              \u{2014} Remove a friend".into(),
            "  FRIEND +nick1 -nick2      \u{2014} Batch add/remove friends".into(),
        ]
    }
}
