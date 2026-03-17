//! NickServ identity module — REGISTER, IDENTIFY, INFO, GHOST/RELEASE.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use subtle::ConstantTimeEq;
use tracing::{info, warn};

use crate::services::module::{CommandContext, ServiceModule};
use crate::services::nickserv::{Identity, NickServState, hash_password, now_unix};

/// Identity management module for NickServ.
pub struct IdentityModule {
    state: Arc<NickServState>,
}

impl IdentityModule {
    pub fn new(state: Arc<NickServState>) -> Self {
        Self { state }
    }

    // -- REGISTER <password> ------------------------------------------------

    async fn cmd_register(&self, ctx: &CommandContext<'_>) {
        let Some(password) = ctx.arg1 else {
            ctx.reply("Usage: REGISTER <password>").await;
            return;
        };

        let nick_lower = ctx.sender.to_ascii_lowercase();

        if self.state.is_registered(&nick_lower).await {
            ctx.reply("This nickname is already registered.").await;
            return;
        }

        let identity = Identity {
            nick: ctx.sender.to_string(),
            password_hash: Some(hash_password(password)),
            pubkey_hex: None,
            registered_at: now_unix(),
            reputation: 0,
            capabilities: Vec::new(),
        };

        self.state.register_identity(identity).await;
        ctx.reply("Nickname registered successfully. You are now identified.")
            .await;
        info!(nick = %ctx.sender, method = "password", "NickServ: registered");
    }

    // -- IDENTIFY <password> ------------------------------------------------

    async fn cmd_identify(&self, ctx: &CommandContext<'_>) {
        let Some(password) = ctx.arg1 else {
            ctx.reply("Usage: IDENTIFY <password>").await;
            return;
        };

        match self.state.get_identity(ctx.sender).await {
            None => {
                ctx.reply("This nickname is not registered.").await;
            }
            Some(identity) => match &identity.password_hash {
                Some(hash)
                    if hash
                        .as_bytes()
                        .ct_eq(hash_password(password).as_bytes())
                        .into() =>
                {
                    ctx.reply("You are now identified.").await;
                    info!(nick = %ctx.sender, "NickServ: identified via password");
                }
                Some(_) => {
                    ctx.reply("Incorrect password.").await;
                    warn!(nick = %ctx.sender, "NickServ: failed password identify");
                }
                None => {
                    ctx.reply("This nick uses keypair auth. Use CHALLENGE/VERIFY.")
                        .await;
                }
            },
        }
    }

    // -- INFO [nick] --------------------------------------------------------

    async fn cmd_info(&self, ctx: &CommandContext<'_>) {
        let target = ctx.arg1.unwrap_or(ctx.sender);

        match self.state.get_identity(target).await {
            None => {
                ctx.reply(&format!("{target} is not registered.")).await;
            }
            Some(identity) => {
                ctx.reply(&format!("Information for \x02{}\x02:", identity.nick))
                    .await;
                let method = if identity.pubkey_hex.is_some() {
                    "keypair"
                } else {
                    "password"
                };
                ctx.reply(&format!("  Auth method: {method}")).await;
                ctx.reply(&format!("  Reputation:  {}", identity.reputation))
                    .await;
                ctx.reply(&format!("  Registered:  {}", identity.registered_at))
                    .await;
                if !identity.capabilities.is_empty() {
                    ctx.reply(&format!(
                        "  Capabilities: {}",
                        identity.capabilities.join(", ")
                    ))
                    .await;
                }
                if let Some(ref pk) = identity.pubkey_hex {
                    ctx.reply(&format!("  Public key:  {pk}")).await;
                }
            }
        }
    }

    // -- GHOST <nick> <password> / RELEASE (alias) ---------------------------

    async fn cmd_ghost(&self, ctx: &CommandContext<'_>) {
        let Some(nick) = ctx.arg1 else {
            ctx.reply("Usage: GHOST <nick> <password>").await;
            return;
        };
        let Some(password) = ctx.arg2 else {
            ctx.reply("Usage: GHOST <nick> <password>").await;
            return;
        };

        let Some(identity) = self.state.get_identity(nick).await else {
            ctx.reply(&format!("{nick} is not a registered nickname."))
                .await;
            return;
        };

        // Verify password.
        match &identity.password_hash {
            Some(hash)
                if hash
                    .as_bytes()
                    .ct_eq(hash_password(password).as_bytes())
                    .into() => {}
            Some(_) => {
                ctx.reply("Incorrect password.").await;
                warn!(nick = %ctx.sender, target = %nick, "NickServ: failed GHOST password");
                return;
            }
            None => {
                ctx.reply("That nick uses keypair auth; GHOST requires a password.")
                    .await;
                return;
            }
        }

        // Forcibly disconnect the client using that nick via SharedState.
        let kill_reason = format!("Killed (NickServ (GHOST command used by {}))", ctx.sender);
        match self.state.shared_state().force_disconnect(nick).await {
            Some((disconnected, peers)) => {
                // Send ERROR to the disconnected client.
                let error_line: Arc<str> = format!(
                    "ERROR :Killed (NickServ (GHOST command used by {}))\r\n",
                    ctx.sender
                )
                .into();
                disconnected.send_line(&error_line);

                // Broadcast QUIT to peers.
                let quit_msg = airc_shared::IrcMessage::quit(Some(&kill_reason))
                    .with_prefix(&disconnected.prefix());
                let quit_line: Arc<str> = quit_msg.serialize().into();
                for peer in &peers {
                    peer.send_line(&quit_line);
                }

                ctx.reply(&format!("Ghost of \x02{}\x02 has been disconnected.", nick))
                    .await;
                info!(
                    sender = %ctx.sender,
                    target = %nick,
                    "NickServ: GHOST disconnected client"
                );
            }
            None => {
                ctx.reply(&format!("{} is not currently online.", nick))
                    .await;
            }
        }
    }
}

impl ServiceModule for IdentityModule {
    fn name(&self) -> &str {
        "identity"
    }

    fn commands(&self) -> &[&str] {
        &["REGISTER", "IDENTIFY", "INFO", "GHOST", "RELEASE"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match ctx.command {
                "REGISTER" => self.cmd_register(ctx).await,
                "IDENTIFY" => self.cmd_identify(ctx).await,
                "INFO" => self.cmd_info(ctx).await,
                "GHOST" | "RELEASE" => self.cmd_ghost(ctx).await,
                _ => return false,
            }
            true
        })
    }

    fn help_lines(&self) -> Vec<String> {
        vec![
            "  REGISTER <password>       \u{2014} Register your nick with a password".into(),
            "  IDENTIFY <password>       \u{2014} Identify to your registered nick".into(),
            "  INFO [nick]               \u{2014} View registration info".into(),
            "  GHOST <nick> <password>   \u{2014} Disconnect a client using your nick".into(),
            "  RELEASE <nick> <password> \u{2014} Alias for GHOST".into(),
        ]
    }
}
