//! NickServ — nickname registration and authentication service.
//!
//! Supports two modes of authentication:
//! - **Password**: `REGISTER <password>` / `IDENTIFY <password>`
//! - **Ed25519 keypair**: `REGISTER-KEY <pubkey-hex>` / `CHALLENGE` / `VERIFY <sig-hex>`
//!
//! Registered identities are persisted to a JSON file.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use airc_shared::{Command, IrcMessage};

use super::ServiceBot;
use crate::client::ClientHandle;
use crate::state::SharedState;

const NICKSERV: &str = "NickServ";
const PERSISTENCE_FILE: &str = "nickserv.json";

// ---------------------------------------------------------------------------
// Stored identity
// ---------------------------------------------------------------------------

/// A registered nick identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// The canonical (original casing) nickname.
    pub nick: String,
    /// Argon2-style password hash (for MVP we store a simple hash — see note).
    /// `None` if the user registered with a keypair only.
    pub password_hash: Option<String>,
    /// Ed25519 public key in hex, if registered via keypair.
    pub pubkey_hex: Option<String>,
    /// Unix timestamp of registration.
    pub registered_at: u64,
    /// Reputation score.
    pub reputation: i64,
    /// Declared capabilities (free-form strings).
    pub capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// Pending challenge for keypair auth
// ---------------------------------------------------------------------------

#[allow(dead_code)] // nick_lower kept for future audit/logging.
struct PendingChallenge {
    nonce: [u8; 32],
    nick_lower: String,
}

// ---------------------------------------------------------------------------
// NickServ inner state
// ---------------------------------------------------------------------------

struct Inner {
    /// Registered identities, keyed by lowercase nick.
    identities: RwLock<HashMap<String, Identity>>,
    /// Active challenges for keypair auth, keyed by client nick (lowercase).
    challenges: RwLock<HashMap<String, PendingChallenge>>,
    /// Rate-limit: key is "sender_lower:action:target_lower", value is unix timestamp of last use.
    rate_limits: RwLock<HashMap<String, u64>>,
}

// ---------------------------------------------------------------------------
// NickServ
// ---------------------------------------------------------------------------

/// The NickServ service bot.
pub struct NickServ {
    inner: Arc<Inner>,
}

impl NickServ {
    pub fn new() -> Self {
        let identities = load_identities().unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                identities: RwLock::new(identities),
                challenges: RwLock::new(HashMap::new()),
                rate_limits: RwLock::new(HashMap::new()),
            }),
        }
    }
}

impl ServiceBot for NickServ {
    fn nick(&self) -> &str {
        NICKSERV
    }

    async fn handle(&self, state: &SharedState, sender: &ClientHandle, text: &str) {
        let parts: Vec<&str> = text.splitn(3, ' ').collect();
        let command = match parts.first() {
            Some(c) => c.to_ascii_uppercase(),
            None => return,
        };
        let arg1 = parts.get(1).copied();
        let arg2 = parts.get(2).copied();

        match command.as_str() {
            "REGISTER" => self.cmd_register(state, sender, arg1).await,
            "IDENTIFY" => self.cmd_identify(state, sender, arg1).await,
            "REGISTER-KEY" => self.cmd_register_key(state, sender, arg1).await,
            "CHALLENGE" => self.cmd_challenge(state, sender).await,
            "VERIFY" => self.cmd_verify(state, sender, arg1).await,
            "INFO" => self.cmd_info(state, sender, arg1).await,
            "GHOST" | "RELEASE" => self.cmd_ghost(state, sender, arg1, arg2).await,
            "VOUCH" => self.cmd_vouch(sender, arg1).await,
            "REPORT" => self.cmd_report(sender, arg1).await,
            "REPUTATION" => self.cmd_reputation(sender, arg1).await,
            "HELP" => self.cmd_help(sender),
            _ => {
                reply(
                    sender,
                    &format!("Unknown command: {command}. Use HELP for a list of commands."),
                );
            }
        }
    }
}

impl NickServ {
    // -- REGISTER <password> ------------------------------------------------

    async fn cmd_register(
        &self,
        _state: &SharedState,
        sender: &ClientHandle,
        password: Option<&str>,
    ) {
        let Some(password) = password else {
            reply(sender, "Usage: REGISTER <password>");
            return;
        };

        let nick_lower = sender.info.nick.to_ascii_lowercase();

        {
            let ids = self.inner.identities.read().await;
            if ids.contains_key(&nick_lower) {
                reply(sender, "This nickname is already registered.");
                return;
            }
        }

        let identity = Identity {
            nick: sender.info.nick.clone(),
            password_hash: Some(simple_hash(password)),
            pubkey_hex: None,
            registered_at: now_unix(),
            reputation: 0,
            capabilities: Vec::new(),
        };

        self.inner
            .identities
            .write()
            .await
            .insert(nick_lower, identity);
        self.persist().await;

        reply(
            sender,
            "Nickname registered successfully. You are now identified.",
        );
        info!(nick = %sender.info.nick, method = "password", "NickServ: registered");
    }

    // -- IDENTIFY <password> ------------------------------------------------

    async fn cmd_identify(
        &self,
        _state: &SharedState,
        sender: &ClientHandle,
        password: Option<&str>,
    ) {
        let Some(password) = password else {
            reply(sender, "Usage: IDENTIFY <password>");
            return;
        };

        let nick_lower = sender.info.nick.to_ascii_lowercase();
        let ids = self.inner.identities.read().await;

        match ids.get(&nick_lower) {
            None => {
                reply(sender, "This nickname is not registered.");
            }
            Some(identity) => match &identity.password_hash {
                Some(hash) if *hash == simple_hash(password) => {
                    reply(sender, "You are now identified.");
                    info!(nick = %sender.info.nick, "NickServ: identified via password");
                }
                Some(_) => {
                    reply(sender, "Incorrect password.");
                    warn!(nick = %sender.info.nick, "NickServ: failed password identify");
                }
                None => {
                    reply(sender, "This nick uses keypair auth. Use CHALLENGE/VERIFY.");
                }
            },
        }
    }

    // -- REGISTER-KEY <pubkey-hex> ------------------------------------------

    async fn cmd_register_key(
        &self,
        _state: &SharedState,
        sender: &ClientHandle,
        pubkey_hex: Option<&str>,
    ) {
        let Some(pubkey_hex) = pubkey_hex else {
            reply(sender, "Usage: REGISTER-KEY <ed25519-public-key-hex>");
            return;
        };

        // Validate the public key.
        if parse_pubkey(pubkey_hex).is_none() {
            reply(
                sender,
                "Invalid Ed25519 public key (expected 64 hex chars).",
            );
            return;
        }

        let nick_lower = sender.info.nick.to_ascii_lowercase();

        {
            let ids = self.inner.identities.read().await;
            if ids.contains_key(&nick_lower) {
                reply(sender, "This nickname is already registered.");
                return;
            }
        }

        let identity = Identity {
            nick: sender.info.nick.clone(),
            password_hash: None,
            pubkey_hex: Some(pubkey_hex.to_string()),
            registered_at: now_unix(),
            reputation: 0,
            capabilities: Vec::new(),
        };

        self.inner
            .identities
            .write()
            .await
            .insert(nick_lower, identity);
        self.persist().await;

        reply(
            sender,
            "Nickname registered with keypair. Use CHALLENGE/VERIFY to identify.",
        );
        info!(nick = %sender.info.nick, method = "keypair", "NickServ: registered");
    }

    // -- CHALLENGE ----------------------------------------------------------

    async fn cmd_challenge(&self, _state: &SharedState, sender: &ClientHandle) {
        let nick_lower = sender.info.nick.to_ascii_lowercase();

        {
            let ids = self.inner.identities.read().await;
            match ids.get(&nick_lower) {
                None => {
                    reply(sender, "This nickname is not registered.");
                    return;
                }
                Some(id) if id.pubkey_hex.is_none() => {
                    reply(
                        sender,
                        "This nick uses password auth. Use IDENTIFY <password>.",
                    );
                    return;
                }
                _ => {}
            }
        }

        let mut nonce = [0u8; 32];
        rand::thread_rng().fill(&mut nonce);
        let nonce_hex = hex_encode(&nonce);

        self.inner
            .challenges
            .write()
            .await
            .insert(nick_lower.clone(), PendingChallenge { nonce, nick_lower });

        reply(sender, &format!("CHALLENGE {nonce_hex}"));
        reply(
            sender,
            "Sign this nonce with your private key and reply: VERIFY <signature-hex>",
        );
        debug!(nick = %sender.info.nick, "NickServ: issued challenge");
    }

    // -- VERIFY <signature-hex> ---------------------------------------------

    async fn cmd_verify(&self, _state: &SharedState, sender: &ClientHandle, sig_hex: Option<&str>) {
        let Some(sig_hex) = sig_hex else {
            reply(sender, "Usage: VERIFY <signature-hex>");
            return;
        };

        let nick_lower = sender.info.nick.to_ascii_lowercase();

        // Get the pending challenge.
        let challenge = self.inner.challenges.write().await.remove(&nick_lower);
        let Some(challenge) = challenge else {
            reply(sender, "No pending challenge. Use CHALLENGE first.");
            return;
        };

        // Get the stored public key.
        let ids = self.inner.identities.read().await;
        let Some(identity) = ids.get(&nick_lower) else {
            reply(sender, "This nickname is not registered.");
            return;
        };
        let Some(ref pubkey_hex) = identity.pubkey_hex else {
            reply(sender, "No public key on file for this nick.");
            return;
        };

        // Parse pubkey and signature, then verify.
        let Some(verifying_key) = parse_pubkey(pubkey_hex) else {
            reply(sender, "Stored public key is invalid (server error).");
            return;
        };

        let Some(sig_bytes) = hex_decode(sig_hex) else {
            reply(sender, "Invalid signature hex.");
            return;
        };

        if sig_bytes.len() != 64 {
            reply(
                sender,
                "Invalid signature length (expected 128 hex chars / 64 bytes).",
            );
            return;
        }

        let signature = Signature::from_bytes(&sig_bytes.try_into().unwrap());

        match verifying_key.verify(&challenge.nonce, &signature) {
            Ok(()) => {
                reply(sender, "Signature verified. You are now identified.");
                info!(nick = %sender.info.nick, "NickServ: identified via keypair");
            }
            Err(_) => {
                reply(sender, "Signature verification failed.");
                warn!(nick = %sender.info.nick, "NickServ: failed keypair verify");
            }
        }
    }

    // -- INFO [nick] --------------------------------------------------------

    async fn cmd_info(&self, _state: &SharedState, sender: &ClientHandle, nick: Option<&str>) {
        let target = nick.unwrap_or(&sender.info.nick);
        let nick_lower = target.to_ascii_lowercase();

        let ids = self.inner.identities.read().await;
        match ids.get(&nick_lower) {
            None => {
                reply(sender, &format!("{target} is not registered."));
            }
            Some(identity) => {
                reply(
                    sender,
                    &format!("Information for \x02{}\x02:", identity.nick),
                );
                let method = if identity.pubkey_hex.is_some() {
                    "keypair"
                } else {
                    "password"
                };
                reply(sender, &format!("  Auth method: {method}"));
                reply(sender, &format!("  Reputation:  {}", identity.reputation));
                reply(
                    sender,
                    &format!("  Registered:  {}", identity.registered_at),
                );
                if !identity.capabilities.is_empty() {
                    reply(
                        sender,
                        &format!("  Capabilities: {}", identity.capabilities.join(", ")),
                    );
                }
                if let Some(ref pk) = identity.pubkey_hex {
                    reply(sender, &format!("  Public key:  {pk}"));
                }
            }
        }
    }

    // -- VOUCH <nick> -------------------------------------------------------

    async fn cmd_vouch(&self, sender: &ClientHandle, target: Option<&str>) {
        let Some(target) = target else {
            reply(sender, "Usage: VOUCH <nick>");
            return;
        };

        let sender_lower = sender.info.nick.to_ascii_lowercase();

        // Sender must be registered.
        if self.get_identity(&sender.info.nick).await.is_none() {
            reply(sender, "You must be registered to vouch for someone.");
            return;
        }

        // Cannot vouch for yourself.
        if target.to_ascii_lowercase() == sender_lower {
            reply(sender, "You cannot vouch for yourself.");
            return;
        }

        // Rate-limit: one vouch per (sender, target) every 5 minutes.
        if let Err(remaining) = self
            .check_rate_limit(&sender.info.nick, "vouch", target)
            .await
        {
            reply(
                sender,
                &format!("Rate limited. Try again in {remaining} seconds."),
            );
            return;
        }

        match self.modify_reputation(target, 1).await {
            Some(new_score) => {
                reply(
                    sender,
                    &format!(
                        "You vouched for \x02{target}\x02. Their reputation is now {new_score}."
                    ),
                );
                info!(sender = %sender.info.nick, target = %target, "NickServ: vouch");
            }
            None => {
                reply(sender, &format!("{target} is not registered."));
            }
        }
    }

    // -- REPORT <nick> ------------------------------------------------------

    async fn cmd_report(&self, sender: &ClientHandle, target: Option<&str>) {
        let Some(target) = target else {
            reply(sender, "Usage: REPORT <nick>");
            return;
        };

        let sender_lower = sender.info.nick.to_ascii_lowercase();

        // Sender must be registered.
        if self.get_identity(&sender.info.nick).await.is_none() {
            reply(sender, "You must be registered to report someone.");
            return;
        }

        // Cannot report yourself.
        if target.to_ascii_lowercase() == sender_lower {
            reply(sender, "You cannot report yourself.");
            return;
        }

        // Rate-limit: one report per (sender, target) every 5 minutes.
        if let Err(remaining) = self
            .check_rate_limit(&sender.info.nick, "report", target)
            .await
        {
            reply(
                sender,
                &format!("Rate limited. Try again in {remaining} seconds."),
            );
            return;
        }

        match self.modify_reputation(target, -1).await {
            Some(new_score) => {
                reply(
                    sender,
                    &format!("You reported \x02{target}\x02. Their reputation is now {new_score}."),
                );
                info!(sender = %sender.info.nick, target = %target, "NickServ: report");
            }
            None => {
                reply(sender, &format!("{target} is not registered."));
            }
        }
    }

    // -- REPUTATION <nick> --------------------------------------------------

    async fn cmd_reputation(&self, sender: &ClientHandle, nick: Option<&str>) {
        let Some(nick) = nick else {
            reply(sender, "Usage: REPUTATION <nick>");
            return;
        };

        match self.get_identity(nick).await {
            Some(identity) => {
                reply(
                    sender,
                    &format!(
                        "Reputation for \x02{}\x02: {}",
                        identity.nick, identity.reputation
                    ),
                );
            }
            None => {
                reply(sender, &format!("{nick} is not registered."));
            }
        }
    }

    // -- GHOST <nick> <password> / RELEASE (alias) ---------------------------

    async fn cmd_ghost(
        &self,
        state: &SharedState,
        sender: &ClientHandle,
        nick: Option<&str>,
        password: Option<&str>,
    ) {
        let Some(nick) = nick else {
            reply(sender, "Usage: GHOST <nick> <password>");
            return;
        };
        let Some(password) = password else {
            reply(sender, "Usage: GHOST <nick> <password>");
            return;
        };

        let nick_lower = nick.to_ascii_lowercase();

        // Look up the registered identity for the target nick.
        let identity = {
            let ids = self.inner.identities.read().await;
            ids.get(&nick_lower).cloned()
        };

        let Some(identity) = identity else {
            reply(sender, &format!("{nick} is not a registered nickname."));
            return;
        };

        // Verify password.
        match &identity.password_hash {
            Some(hash) if *hash == simple_hash(password) => {}
            Some(_) => {
                reply(sender, "Incorrect password.");
                warn!(nick = %sender.info.nick, target = %nick, "NickServ: failed GHOST password");
                return;
            }
            None => {
                reply(
                    sender,
                    "That nick uses keypair auth; GHOST requires a password.",
                );
                return;
            }
        }

        // Disconnect the client currently using that nick.
        let Some((target_handle, peers)) = state.force_disconnect(nick).await else {
            reply(sender, &format!("{nick} is not currently in use."));
            return;
        };

        // Send ERROR to the ghosted client.
        let error_msg = IrcMessage {
            prefix: None,
            command: Command::Unknown("ERROR".to_string()),
            params: vec![format!(
                "Closing Link: {} (Killed (NickServ (GHOST command used by {})))",
                target_handle.info.hostname, sender.info.nick
            )],
        };
        target_handle.send_message(&error_msg);

        // Send QUIT to all their channel peers.
        let quit_msg = IrcMessage::quit(Some(&format!(
            "Killed (NickServ (GHOST command used by {}))",
            sender.info.nick
        )))
        .with_prefix(target_handle.prefix());
        for peer in &peers {
            peer.send_message(&quit_msg);
        }

        reply(
            sender,
            &format!("Ghost of \x02{}\x02 has been disconnected.", nick),
        );
        info!(
            sender = %sender.info.nick,
            target = %nick,
            "NickServ: GHOST disconnected client"
        );
    }

    // -- HELP ---------------------------------------------------------------

    fn cmd_help(&self, sender: &ClientHandle) {
        let lines = [
            "NickServ commands:",
            "  REGISTER <password>       — Register your nick with a password",
            "  IDENTIFY <password>       — Identify to your registered nick",
            "  REGISTER-KEY <pubkey-hex> — Register with an Ed25519 public key",
            "  CHALLENGE                 — Request a nonce for keypair auth",
            "  VERIFY <signature-hex>    — Prove identity with a signed nonce",
            "  GHOST <nick> <password>   — Disconnect a client using your nick",
            "  RELEASE <nick> <password> — Alias for GHOST",
            "  INFO [nick]               — View registration info",
            "  VOUCH <nick>              — Give +1 reputation to a nick",
            "  REPORT <nick>             — Give -1 reputation to a nick",
            "  REPUTATION <nick>         — Query a nick's reputation score",
            "  HELP                      — Show this help",
        ];
        for line in &lines {
            reply(sender, line);
        }
    }

    // -- Rate-limit helper ---------------------------------------------------

    /// Check and record a rate-limited action.
    /// Returns `Ok(())` if allowed, `Err(seconds_remaining)` if too soon.
    async fn check_rate_limit(&self, sender: &str, action: &str, target: &str) -> Result<(), u64> {
        let key = format!(
            "{}:{}:{}",
            sender.to_ascii_lowercase(),
            action,
            target.to_ascii_lowercase()
        );
        let now = now_unix();
        let cooldown = 300; // 5 minutes

        let mut limits = self.inner.rate_limits.write().await;
        if let Some(&last) = limits.get(&key) {
            let elapsed = now.saturating_sub(last);
            if elapsed < cooldown {
                return Err(cooldown - elapsed);
            }
        }
        limits.insert(key, now);
        Ok(())
    }

    // -- Persistence --------------------------------------------------------

    async fn persist(&self) {
        let ids = self.inner.identities.read().await;
        if let Err(e) = save_identities(&ids) {
            warn!(error = %e, "NickServ: failed to persist identities");
        }
    }

    // -- Public query for reputation (used by handler for WHOIS, etc.) ------

    /// Look up a registered identity by nick (case-insensitive).
    pub async fn get_identity(&self, nick: &str) -> Option<Identity> {
        let ids = self.inner.identities.read().await;
        ids.get(&nick.to_ascii_lowercase()).cloned()
    }

    /// Modify reputation for a nick. Returns the new score, or None if not registered.
    pub async fn modify_reputation(&self, nick: &str, delta: i64) -> Option<i64> {
        let nick_lower = nick.to_ascii_lowercase();
        let mut ids = self.inner.identities.write().await;
        let identity = ids.get_mut(&nick_lower)?;
        identity.reputation += delta;
        let new_score = identity.reputation;
        drop(ids);
        // Persist in background (best-effort).
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let ids = inner.identities.read().await;
            if let Err(e) = save_identities(&ids) {
                warn!(error = %e, "NickServ: failed to persist after reputation change");
            }
        });
        Some(new_score)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Send a NOTICE from NickServ to a client.
fn reply(client: &ClientHandle, text: &str) {
    let msg = IrcMessage::notice(&client.info.nick, text).with_prefix(NICKSERV);
    client.send_message(&msg);
}

/// Trivial hash for MVP. In production, use argon2 or bcrypt.
fn simple_hash(password: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    password.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

fn parse_pubkey(hex: &str) -> Option<VerifyingKey> {
    let bytes = hex_decode(hex)?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

// ---------------------------------------------------------------------------
// Persistence (JSON file)
// ---------------------------------------------------------------------------

fn load_identities() -> Result<HashMap<String, Identity>, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(PERSISTENCE_FILE)?;
    let map: HashMap<String, Identity> = serde_json::from_str(&data)?;
    info!(count = map.len(), "NickServ: loaded identities");
    Ok(map)
}

fn save_identities(ids: &HashMap<String, Identity>) -> Result<(), Box<dyn std::error::Error>> {
    let data = serde_json::to_string_pretty(ids)?;
    std::fs::write(PERSISTENCE_FILE, data)?;
    debug!(count = ids.len(), "NickServ: persisted identities");
    Ok(())
}
