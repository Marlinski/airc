//! NickServ keypair module — REGISTER-KEY, CHALLENGE, VERIFY.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ed25519_dalek::{Signature, Verifier};
use rand::Rng;
use tracing::{debug, info, warn};

use crate::module::{CommandContext, ServiceModule};
use crate::nickserv::{
    Identity, NickServState, PendingChallenge, hex_decode, hex_encode, now_unix, parse_pubkey,
};

/// Ed25519 keypair authentication module for NickServ.
pub struct KeypairModule {
    state: Arc<NickServState>,
}

impl KeypairModule {
    pub fn new(state: Arc<NickServState>) -> Self {
        Self { state }
    }

    // -- REGISTER-KEY <pubkey-hex> ------------------------------------------

    async fn cmd_register_key(&self, ctx: &CommandContext<'_>) {
        let Some(pubkey_hex) = ctx.arg1 else {
            ctx.reply("Usage: REGISTER-KEY <ed25519-public-key-hex>")
                .await;
            return;
        };

        // Validate the public key.
        if parse_pubkey(pubkey_hex).is_none() {
            ctx.reply("Invalid Ed25519 public key (expected 64 hex chars).")
                .await;
            return;
        }

        if self.state.is_registered(ctx.sender).await {
            ctx.reply("This nickname is already registered.").await;
            return;
        }

        let identity = Identity {
            nick: ctx.sender.to_string(),
            password_hash: None,
            pubkey_hex: Some(pubkey_hex.to_string()),
            registered_at: now_unix(),
            reputation: 0,
            capabilities: Vec::new(),
        };

        self.state.register_identity(identity).await;
        ctx.reply("Nickname registered with keypair. Use CHALLENGE/VERIFY to identify.")
            .await;
        info!(nick = %ctx.sender, method = "keypair", "NickServ: registered");
    }

    // -- CHALLENGE ----------------------------------------------------------

    async fn cmd_challenge(&self, ctx: &CommandContext<'_>) {
        match self.state.get_pubkey_hex(ctx.sender).await {
            None => {
                ctx.reply("This nickname is not registered.").await;
                return;
            }
            Some(None) => {
                ctx.reply("This nick uses password auth. Use IDENTIFY <password>.")
                    .await;
                return;
            }
            Some(Some(_)) => {}
        }

        let mut nonce = [0u8; 32];
        rand::thread_rng().fill(&mut nonce);
        let nonce_hex = hex_encode(&nonce);
        let nick_lower = ctx.sender.to_ascii_lowercase();

        self.state
            .set_challenge(ctx.sender, PendingChallenge { nonce, nick_lower })
            .await;

        ctx.reply(&format!("CHALLENGE {nonce_hex}")).await;
        ctx.reply("Sign this nonce with your private key and reply: VERIFY <signature-hex>")
            .await;
        debug!(nick = %ctx.sender, "NickServ: issued challenge");
    }

    // -- VERIFY <signature-hex> ---------------------------------------------

    async fn cmd_verify(&self, ctx: &CommandContext<'_>) {
        let Some(sig_hex) = ctx.arg1 else {
            ctx.reply("Usage: VERIFY <signature-hex>").await;
            return;
        };

        // Get the pending challenge.
        let Some(challenge) = self.state.take_challenge(ctx.sender).await else {
            ctx.reply("No pending challenge. Use CHALLENGE first.")
                .await;
            return;
        };

        // Get the stored public key.
        let Some(identity) = self.state.get_identity(ctx.sender).await else {
            ctx.reply("This nickname is not registered.").await;
            return;
        };
        let Some(ref pubkey_hex) = identity.pubkey_hex else {
            ctx.reply("No public key on file for this nick.").await;
            return;
        };

        // Parse pubkey and signature, then verify.
        let Some(verifying_key) = parse_pubkey(pubkey_hex) else {
            ctx.reply("Stored public key is invalid (server error).")
                .await;
            return;
        };

        let Some(sig_bytes) = hex_decode(sig_hex) else {
            ctx.reply("Invalid signature hex.").await;
            return;
        };

        if sig_bytes.len() != 64 {
            ctx.reply("Invalid signature length (expected 128 hex chars / 64 bytes).")
                .await;
            return;
        }

        let signature = Signature::from_bytes(&sig_bytes.try_into().unwrap());

        match verifying_key.verify(&challenge.nonce, &signature) {
            Ok(()) => {
                ctx.reply("Signature verified. You are now identified.")
                    .await;
                info!(nick = %ctx.sender, "NickServ: identified via keypair");
            }
            Err(_) => {
                ctx.reply("Signature verification failed.").await;
                warn!(nick = %ctx.sender, "NickServ: failed keypair verify");
            }
        }
    }
}

impl ServiceModule for KeypairModule {
    fn name(&self) -> &str {
        "keypair"
    }

    fn commands(&self) -> &[&str] {
        &["REGISTER-KEY", "CHALLENGE", "VERIFY"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match ctx.command {
                "REGISTER-KEY" => self.cmd_register_key(ctx).await,
                "CHALLENGE" => self.cmd_challenge(ctx).await,
                "VERIFY" => self.cmd_verify(ctx).await,
                _ => return false,
            }
            true
        })
    }

    fn help_lines(&self) -> Vec<String> {
        vec![
            "  REGISTER-KEY <pubkey-hex> \u{2014} Register with an Ed25519 public key".into(),
            "  CHALLENGE                 \u{2014} Request a nonce for keypair auth".into(),
            "  VERIFY <signature-hex>    \u{2014} Prove identity with a signed nonce".into(),
        ]
    }
}
