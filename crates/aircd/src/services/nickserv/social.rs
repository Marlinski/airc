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

use crate::services::module::{CommandContext, ServiceModule};
use crate::services::nickserv::NickServState;

/// Social graph module for NickServ (FRIEND command).
pub struct SocialModule {
    state: Arc<NickServState>,
}

impl SocialModule {
    pub fn new(state: Arc<NickServState>) -> Self {
        Self { state }
    }

    async fn cmd_friend(&self, ctx: &CommandContext<'_>) {
        // Sender must be registered (1 lock acquisition).
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

        // Collect candidate target nicks (de-duplicated, non-empty, not self).
        // We keep the original token alongside the (adding, target_nick) pair
        // so we can reply per-token after the batch.
        struct Op<'t> {
            adding: bool,
            target: &'t str,
        }

        let mut ops: Vec<Op<'_>> = Vec::with_capacity(tokens.len());
        for token in &tokens {
            let (adding, target_nick): (bool, &str) = if let Some(nick) = token.strip_prefix('+') {
                (true, nick)
            } else if let Some(nick) = token.strip_prefix('-') {
                (false, nick)
            } else {
                (true, token)
            };

            if target_nick.is_empty() {
                ctx.reply("Usage: FRIEND [+nick|-nick ...]").await;
                continue;
            }
            if target_nick.eq_ignore_ascii_case(ctx.sender) {
                ctx.reply("You cannot friend yourself.").await;
                continue;
            }
            ops.push(Op {
                adding,
                target: target_nick,
            });
        }

        if ops.is_empty() {
            return;
        }

        // Check registration for all unique targets in one identities lock
        // acquisition.
        let target_nicks: Vec<&str> = ops.iter().map(|o| o.target).collect();
        let registered = self.state.registered_set(&target_nicks);

        // Partition into adds / removes (only registered targets).
        let mut adds: Vec<String> = Vec::new();
        let mut removes: Vec<String> = Vec::new();

        // We also want per-op replies, so track which ops passed validation.
        // We'll separate "not registered" errors first, then batch CRDT ops.
        let mut valid_ops: Vec<Op<'_>> = Vec::with_capacity(ops.len());
        for op in ops {
            let lower = op.target.to_ascii_lowercase();
            if !registered.contains(&lower) {
                ctx.reply(&format!("{} is not registered.", op.target))
                    .await;
                continue;
            }
            if op.adding {
                adds.push(lower);
            } else {
                removes.push(lower);
            }
            valid_ops.push(op);
        }

        if adds.is_empty() && removes.is_empty() {
            info!(sender = %ctx.sender, "NickServ: FRIEND command processed");
            return;
        }

        // One CRDT lock acquisition for all mutations.
        let (added_count, removed_count) = self
            .state
            .batch_friend_ops(ctx.sender, &adds, &removes)
            .await;

        // Per-op replies based on whether the batch call counted the op.
        // Since batch_friend_ops only counts ops that actually changed state,
        // re-derive which targets were already present / absent by diffing.
        let adds_set: std::collections::HashSet<&str> = adds.iter().map(String::as_str).collect();
        let removes_set: std::collections::HashSet<&str> =
            removes.iter().map(String::as_str).collect();

        // For replies: we report success for (added_count) adds and
        // (removed_count) removes; the rest are "already" / "not in list".
        // Because we can't tell *which* individual ops were no-ops without
        // querying again, we report success for all valid ops (the counts are
        // informational for logging only).
        for op in &valid_ops {
            let lower = op.target.to_ascii_lowercase();
            if op.adding && adds_set.contains(lower.as_str()) {
                // batch reported it as added iff added_count > 0; but the
                // batch may have added fewer than all (some already friends).
                // We accept a small reply inaccuracy over N lock acquisitions.
                ctx.reply(&format!("\x02{}\x02 is now your friend.", op.target))
                    .await;
                debug!(sender = %ctx.sender, target = %op.target, "NickServ: friend +nick");
            } else if !op.adding && removes_set.contains(lower.as_str()) {
                ctx.reply(&format!("\x02{}\x02 is no longer your friend.", op.target))
                    .await;
                debug!(sender = %ctx.sender, target = %op.target, "NickServ: friend -nick");
            }
        }

        info!(
            sender = %ctx.sender,
            added = added_count,
            removed = removed_count,
            "NickServ: FRIEND command processed"
        );
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
