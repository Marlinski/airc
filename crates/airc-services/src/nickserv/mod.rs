//! NickServ — nickname registration and authentication service.
//!
//! Composed of togglable modules:
//! - **identity**: REGISTER, IDENTIFY, INFO, GHOST/RELEASE
//! - **keypair**: REGISTER-KEY, CHALLENGE, VERIFY
//! - **reputation**: VOUCH, REPORT, REPUTATION
//! - **social**: FRIEND (social graph, moved from aircd)

pub mod identity;
pub mod keypair;
pub mod reputation;
pub mod social;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::NickServModules;
use crate::module::{ServiceDispatcher, ServiceModule};

const PERSISTENCE_FILE: &str = "nickserv.json";

// ---------------------------------------------------------------------------
// Stored identity
// ---------------------------------------------------------------------------

/// A registered nick identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// The canonical (original casing) nickname.
    pub nick: String,
    /// Password hash (for MVP we store a simple hash — see note).
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
pub(crate) struct PendingChallenge {
    pub nonce: [u8; 32],
    pub nick_lower: String,
}

// ---------------------------------------------------------------------------
// Friend entry (persisted)
// ---------------------------------------------------------------------------

/// A friend relationship stored per-identity.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FriendList {
    /// Set of lowercase nicks this identity has friended.
    pub friends: Vec<String>,
}

// ---------------------------------------------------------------------------
// NickServ shared state
// ---------------------------------------------------------------------------

/// Shared inner state for all NickServ modules.
///
/// All modules within NickServ share an `Arc<NickServState>`. The state
/// exposes methods like `get_identity()`, `modify_reputation()`, `persist()`
/// rather than having modules lock the HashMap directly.
pub struct NickServState {
    /// Registered identities, keyed by lowercase nick.
    identities: RwLock<HashMap<String, Identity>>,
    /// Active challenges for keypair auth, keyed by client nick (lowercase).
    challenges: RwLock<HashMap<String, PendingChallenge>>,
    /// Rate-limit: key is "sender_lower:action:target_lower", value is unix timestamp.
    rate_limits: RwLock<HashMap<String, u64>>,
    /// Per-identity friend lists, keyed by lowercase nick.
    friend_lists: RwLock<HashMap<String, FriendList>>,
    /// Directory for persistence files.
    data_dir: PathBuf,
    /// IRC client connection for raw line sends (e.g. KILL for GHOST).
    client: airc_client::IrcClient,
}

impl NickServState {
    /// Create a new NickServState, loading persisted data from disk.
    pub fn new(client: airc_client::IrcClient, data_dir: &Path) -> Self {
        let persistence_path = data_dir.join(PERSISTENCE_FILE);
        let identities = load_identities(&persistence_path).unwrap_or_default();

        let friends_path = data_dir.join("nickserv_friends.json");
        let friend_lists = load_friend_lists(&friends_path).unwrap_or_default();

        Self {
            identities: RwLock::new(identities),
            challenges: RwLock::new(HashMap::new()),
            rate_limits: RwLock::new(HashMap::new()),
            friend_lists: RwLock::new(friend_lists),
            data_dir: data_dir.to_path_buf(),
            client,
        }
    }

    // -- Identity queries ---------------------------------------------------

    /// Look up a registered identity by nick (case-insensitive).
    pub async fn get_identity(&self, nick: &str) -> Option<Identity> {
        let ids = self.identities.read().await;
        ids.get(&nick.to_ascii_lowercase()).cloned()
    }

    /// Check if a nick is registered.
    pub async fn is_registered(&self, nick: &str) -> bool {
        self.identities
            .read()
            .await
            .contains_key(&nick.to_ascii_lowercase())
    }

    /// Insert a new identity. Returns `false` if the nick is already registered.
    pub async fn register_identity(&self, identity: Identity) -> bool {
        let nick_lower = identity.nick.to_ascii_lowercase();
        let mut ids = self.identities.write().await;
        if ids.contains_key(&nick_lower) {
            return false;
        }
        ids.insert(nick_lower, identity);
        drop(ids);
        self.persist_identities().await;
        true
    }

    /// Get the password hash for a nick, if it has one.
    #[allow(dead_code)]
    pub async fn get_password_hash(&self, nick: &str) -> Option<Option<String>> {
        let ids = self.identities.read().await;
        ids.get(&nick.to_ascii_lowercase())
            .map(|id| id.password_hash.clone())
    }

    /// Get the public key hex for a nick, if registered.
    pub async fn get_pubkey_hex(&self, nick: &str) -> Option<Option<String>> {
        let ids = self.identities.read().await;
        ids.get(&nick.to_ascii_lowercase())
            .map(|id| id.pubkey_hex.clone())
    }

    // -- Reputation ---------------------------------------------------------

    /// Modify reputation for a nick. Returns the new score, or None if not registered.
    pub async fn modify_reputation(&self, nick: &str, delta: i64) -> Option<i64> {
        let nick_lower = nick.to_ascii_lowercase();
        let mut ids = self.identities.write().await;
        let identity = ids.get_mut(&nick_lower)?;
        identity.reputation += delta;
        let new_score = identity.reputation;
        drop(ids);
        self.persist_identities().await;
        Some(new_score)
    }

    // -- Challenges ---------------------------------------------------------

    /// Store a pending challenge for keypair auth.
    pub async fn set_challenge(&self, nick: &str, challenge: PendingChallenge) {
        self.challenges
            .write()
            .await
            .insert(nick.to_ascii_lowercase(), challenge);
    }

    /// Remove and return a pending challenge.
    pub async fn take_challenge(&self, nick: &str) -> Option<PendingChallenge> {
        self.challenges
            .write()
            .await
            .remove(&nick.to_ascii_lowercase())
    }

    // -- Rate limiting ------------------------------------------------------

    /// Check and record a rate-limited action.
    /// Returns `Ok(())` if allowed, `Err(seconds_remaining)` if too soon.
    pub async fn check_rate_limit(
        &self,
        sender: &str,
        action: &str,
        target: &str,
    ) -> Result<(), u64> {
        let key = format!(
            "{}:{}:{}",
            sender.to_ascii_lowercase(),
            action,
            target.to_ascii_lowercase()
        );
        let now = now_unix();
        let cooldown = 300; // 5 minutes

        let mut limits = self.rate_limits.write().await;
        if let Some(&last) = limits.get(&key) {
            let elapsed = now.saturating_sub(last);
            if elapsed < cooldown {
                return Err(cooldown - elapsed);
            }
        }
        limits.insert(key, now);
        Ok(())
    }

    // -- Friend lists -------------------------------------------------------

    /// Send a raw IRC line (e.g. KILL for GHOST command).
    pub async fn send_raw_line(&self, line: &str) -> Result<(), String> {
        self.client.send_line(line).await.map_err(|e| e.to_string())
    }

    /// Add a friend to an identity's friend list.
    pub async fn add_friend(&self, nick: &str, friend_nick: &str) -> bool {
        let nick_lower = nick.to_ascii_lowercase();
        let friend_lower = friend_nick.to_ascii_lowercase();
        let mut lists = self.friend_lists.write().await;
        let list = lists.entry(nick_lower).or_default();
        if list.friends.contains(&friend_lower) {
            return false; // already friends
        }
        list.friends.push(friend_lower);
        drop(lists);
        self.persist_friends().await;
        true
    }

    /// Remove a friend from an identity's friend list.
    pub async fn remove_friend(&self, nick: &str, friend_nick: &str) -> bool {
        let nick_lower = nick.to_ascii_lowercase();
        let friend_lower = friend_nick.to_ascii_lowercase();
        let mut lists = self.friend_lists.write().await;
        let Some(list) = lists.get_mut(&nick_lower) else {
            return false;
        };
        let before = list.friends.len();
        list.friends.retain(|f| *f != friend_lower);
        let removed = list.friends.len() < before;
        if list.friends.is_empty() {
            lists.remove(&nick_lower);
        }
        drop(lists);
        if removed {
            self.persist_friends().await;
        }
        removed
    }

    /// Get a friend list for a nick.
    pub async fn get_friend_list(&self, nick: &str) -> Vec<String> {
        let lists = self.friend_lists.read().await;
        lists
            .get(&nick.to_ascii_lowercase())
            .map(|fl| fl.friends.clone())
            .unwrap_or_default()
    }

    // -- Persistence --------------------------------------------------------

    async fn persist_identities(&self) {
        let ids = self.identities.read().await;
        let path = self.data_dir.join(PERSISTENCE_FILE);
        if let Err(e) = save_identities(&path, &ids) {
            warn!(error = %e, "NickServ: failed to persist identities");
        }
    }

    async fn persist_friends(&self) {
        let lists = self.friend_lists.read().await;
        let path = self.data_dir.join("nickserv_friends.json");
        if let Err(e) = save_friend_lists(&path, &lists) {
            warn!(error = %e, "NickServ: failed to persist friend lists");
        }
    }
}

// ---------------------------------------------------------------------------
// Module builder
// ---------------------------------------------------------------------------

/// Build the set of NickServ modules based on config toggles.
pub fn build_modules(
    state: Arc<NickServState>,
    modules_cfg: &NickServModules,
) -> Vec<Box<dyn ServiceModule>> {
    let mut modules: Vec<Box<dyn ServiceModule>> = Vec::new();

    if modules_cfg.identity {
        modules.push(Box::new(identity::IdentityModule::new(state.clone())));
    }
    if modules_cfg.keypair {
        modules.push(Box::new(keypair::KeypairModule::new(state.clone())));
    }
    if modules_cfg.reputation {
        modules.push(Box::new(reputation::ReputationModule::new(state.clone())));
    }
    if modules_cfg.social {
        modules.push(Box::new(social::SocialModule::new(state.clone())));
    }

    modules
}

/// Create a fully-wired NickServ dispatcher.
pub fn create_dispatcher(
    state: Arc<NickServState>,
    modules_cfg: &NickServModules,
    client: &airc_client::IrcClient,
) -> ServiceDispatcher {
    let modules = build_modules(state, modules_cfg);
    ServiceDispatcher::new("NickServ".to_string(), modules, client)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Trivial hash for MVP. In production, use argon2 or bcrypt.
pub(crate) fn simple_hash(password: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    password.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

pub(crate) fn parse_pubkey(hex: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes = hex_decode(hex)?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&arr).ok()
}

// ---------------------------------------------------------------------------
// Persistence (JSON files)
// ---------------------------------------------------------------------------

fn load_identities(path: &Path) -> Result<HashMap<String, Identity>, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(path)?;
    let map: HashMap<String, Identity> = serde_json::from_str(&data)?;
    info!(count = map.len(), path = %path.display(), "NickServ: loaded identities");
    Ok(map)
}

fn save_identities(
    path: &Path,
    ids: &HashMap<String, Identity>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = serde_json::to_string_pretty(ids)?;
    std::fs::write(path, data)?;
    debug!(count = ids.len(), path = %path.display(), "NickServ: persisted identities");
    Ok(())
}

fn load_friend_lists(
    path: &Path,
) -> Result<HashMap<String, FriendList>, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(path)?;
    let map: HashMap<String, FriendList> = serde_json::from_str(&data)?;
    info!(count = map.len(), path = %path.display(), "NickServ: loaded friend lists");
    Ok(map)
}

fn save_friend_lists(
    path: &Path,
    lists: &HashMap<String, FriendList>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = serde_json::to_string_pretty(lists)?;
    std::fs::write(path, data)?;
    debug!(count = lists.len(), path = %path.display(), "NickServ: persisted friend lists");
    Ok(())
}
