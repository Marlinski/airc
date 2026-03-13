//! Shared server state — the single source of truth for clients and channels.
//!
//! All mutations flow through [`SharedState`] methods so the interface can be
//! extracted into a trait later for testing or alternative backends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use thiserror::Error;

use airc_shared::{Command, IrcMessage};

use crate::channel::Channel;
use crate::client::{ClientHandle, ClientId, ClientInfo};
use crate::config::ServerConfig;
use crate::logger::ChannelLogger;
use crate::services::ServiceRouter;
use crate::web;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors related to nickname operations.
#[derive(Debug, Clone, Error)]
pub enum NickError {
    #[error("nickname already in use")]
    InUse,
    #[error("invalid nickname")]
    Invalid,
}

// ---------------------------------------------------------------------------
// Inner state (behind the Arc)
// ---------------------------------------------------------------------------

struct Inner {
    clients: RwLock<HashMap<ClientId, ClientHandle>>,
    channels: RwLock<HashMap<String, Channel>>,
    nick_to_id: RwLock<HashMap<String, ClientId>>,
    next_id: AtomicU64,
    config: ServerConfig,
    services: ServiceRouter,
    logger: ChannelLogger,
    started_at: Instant,
}

// ---------------------------------------------------------------------------
// SharedState
// ---------------------------------------------------------------------------

/// Thread-safe, cheaply cloneable handle to all server state.
#[derive(Clone)]
pub struct SharedState {
    inner: Arc<Inner>,
}

impl SharedState {
    /// Create a fresh server state from the given config.
    pub fn new(config: ServerConfig, log_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                clients: RwLock::new(HashMap::new()),
                channels: RwLock::new(HashMap::new()),
                nick_to_id: RwLock::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                config,
                services: ServiceRouter::new(),
                logger: ChannelLogger::new(log_dir),
                started_at: Instant::now(),
            }),
        }
    }

    // -- Identity -----------------------------------------------------------

    /// Allocate the next unique client ID.
    pub fn next_client_id(&self) -> ClientId {
        ClientId(self.inner.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The server's configured hostname.
    pub fn server_name(&self) -> &str {
        &self.inner.config.server_name
    }

    /// Borrow the full server config.
    pub fn config(&self) -> &ServerConfig {
        &self.inner.config
    }

    /// Access the service router (NickServ, ChanServ, etc.).
    pub fn services(&self) -> &ServiceRouter {
        &self.inner.services
    }

    /// Access the channel logger.
    pub fn logger(&self) -> &ChannelLogger {
        &self.inner.logger
    }

    // -- Client management --------------------------------------------------

    /// Register a fully-connected client handle in the shared state.
    #[allow(dead_code)] // Available for future direct-registration paths.
    pub async fn add_client(&self, handle: ClientHandle) {
        let id = handle.id;
        let nick_lower = handle.info.nick.to_ascii_lowercase();
        self.inner.clients.write().await.insert(id, handle);
        self.inner.nick_to_id.write().await.insert(nick_lower, id);
    }

    /// Remove a client from all state (client map, nick map, all channels).
    /// Returns the removed handle if it existed.
    pub async fn remove_client(&self, id: ClientId) -> Option<ClientHandle> {
        let handle = self.inner.clients.write().await.remove(&id);
        if let Some(ref h) = handle {
            let nick_lower = h.info.nick.to_ascii_lowercase();
            self.inner.nick_to_id.write().await.remove(&nick_lower);
        }
        // Remove from every channel.
        let mut channels = self.inner.channels.write().await;
        let empty_channels: Vec<String> = channels
            .iter_mut()
            .filter_map(|(name, ch)| {
                ch.remove_member(id);
                if ch.members.is_empty() {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();
        for name in empty_channels {
            channels.remove(&name);
        }
        handle
    }

    /// Find a client by nickname (case-insensitive). Returns a cloned handle.
    pub async fn find_client_by_nick(&self, nick: &str) -> Option<ClientHandle> {
        let nick_lower = nick.to_ascii_lowercase();
        let id = self.inner.nick_to_id.read().await.get(&nick_lower).copied();
        match id {
            Some(id) => self.inner.clients.read().await.get(&id).cloned(),
            None => None,
        }
    }

    /// Get a client by ID. Returns a cloned handle.
    pub async fn get_client(&self, id: ClientId) -> Option<ClientHandle> {
        self.inner.clients.read().await.get(&id).cloned()
    }

    /// Attempt to change a registered client's nickname.
    ///
    /// Validates the new nick, checks for uniqueness, and updates all maps.
    pub async fn update_nick(&self, id: ClientId, new_nick: &str) -> Result<(), NickError> {
        if !is_valid_nick(new_nick) {
            return Err(NickError::Invalid);
        }
        let new_lower = new_nick.to_ascii_lowercase();

        // Check uniqueness — must not collide with another client.
        {
            let nick_map = self.inner.nick_to_id.read().await;
            if let Some(&existing_id) = nick_map.get(&new_lower) {
                if existing_id != id {
                    return Err(NickError::InUse);
                }
            }
        }

        // Perform the swap.
        let mut clients = self.inner.clients.write().await;
        let mut nick_map = self.inner.nick_to_id.write().await;
        if let Some(handle) = clients.get_mut(&id) {
            let old_lower = handle.info.nick.to_ascii_lowercase();
            nick_map.remove(&old_lower);
            handle.info.nick = new_nick.to_string();
            nick_map.insert(new_lower, id);
        }
        Ok(())
    }

    /// Check nick availability, reserve it, and mark the client as registered.
    ///
    /// This is called once during the registration handshake after both NICK
    /// and USER have been received. It creates the `ClientHandle`, inserts it
    /// into state, and returns a clone.
    pub async fn register_client(
        &self,
        id: ClientId,
        nick: &str,
        username: &str,
        realname: &str,
        hostname: &str,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<ClientHandle, NickError> {
        if !is_valid_nick(nick) {
            return Err(NickError::Invalid);
        }
        let nick_lower = nick.to_ascii_lowercase();

        // Check uniqueness.
        {
            let nick_map = self.inner.nick_to_id.read().await;
            if nick_map.contains_key(&nick_lower) {
                return Err(NickError::InUse);
            }
        }

        let info = ClientInfo {
            nick: nick.to_string(),
            username: username.to_string(),
            realname: realname.to_string(),
            hostname: hostname.to_string(),
            registered: true,
            identified: false,
            modes: String::new(),
        };

        let handle = ClientHandle::new(id, info, tx, self.server_name().to_string());

        self.inner.clients.write().await.insert(id, handle.clone());
        self.inner.nick_to_id.write().await.insert(nick_lower, id);

        Ok(handle)
    }

    // -- Channel management -------------------------------------------------

    /// Join a client to a channel. Creates the channel if it doesn't exist and
    /// makes the joiner an operator. Returns the `Channel` snapshot and list of
    /// member handles (for broadcasting the JOIN).
    pub async fn join_channel(
        &self,
        id: ClientId,
        channel_name: &str,
    ) -> (Channel, Vec<ClientHandle>) {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        let channel = channels
            .entry(key)
            .or_insert_with(|| Channel::new(channel_name.to_string()));

        let is_new_channel = channel.members.is_empty();
        channel.add_member(id);
        if is_new_channel {
            channel.operators.insert(id);
        }

        let snapshot = channel.clone();
        drop(channels);

        // Collect handles for all members (for broadcasting).
        let clients = self.inner.clients.read().await;
        let handles: Vec<ClientHandle> = snapshot
            .members
            .iter()
            .filter_map(|mid| clients.get(mid).cloned())
            .collect();

        (snapshot, handles)
    }

    /// Remove a client from a channel. Returns `None` if the client was not a
    /// member. Otherwise returns a snapshot of remaining member handles.
    pub async fn part_channel(
        &self,
        id: ClientId,
        channel_name: &str,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        let channel = channels.get_mut(&key)?;

        if !channel.remove_member(id) {
            return None;
        }

        let remaining_ids: Vec<ClientId> = channel.member_list();
        let is_empty = remaining_ids.is_empty();

        if is_empty {
            channels.remove(&key);
            return Some(vec![]);
        }

        drop(channels);

        let clients = self.inner.clients.read().await;
        let handles = remaining_ids
            .iter()
            .filter_map(|mid| clients.get(mid).cloned())
            .collect();

        Some(handles)
    }

    /// Get a snapshot of a channel (if it exists).
    pub async fn get_channel(&self, channel_name: &str) -> Option<Channel> {
        let key = channel_name.to_ascii_lowercase();
        self.inner.channels.read().await.get(&key).cloned()
    }

    /// Set the topic on a channel. Returns the member handles for broadcasting.
    pub async fn set_channel_topic(
        &self,
        channel_name: &str,
        text: String,
        setter: String,
        timestamp: u64,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        let channel = channels.get_mut(&key)?;
        channel.set_topic(text, setter, timestamp);
        let member_ids = channel.member_list();
        drop(channels);

        let clients = self.inner.clients.read().await;
        Some(
            member_ids
                .iter()
                .filter_map(|id| clients.get(id).cloned())
                .collect(),
        )
    }

    /// List all channel names with their member counts and topics.
    pub async fn list_channels(&self) -> Vec<(String, usize, Option<String>)> {
        let channels = self.inner.channels.read().await;
        channels
            .values()
            .map(|ch| {
                let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
                (ch.name.clone(), ch.member_count(), topic_text)
            })
            .collect()
    }

    /// Get member handles for a channel.
    pub async fn channel_members(&self, channel_name: &str) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        let channel = channels.get(&key)?;
        let ids = channel.member_list();
        let is_op: Vec<bool> = ids.iter().map(|id| channel.is_operator(*id)).collect();
        drop(channels);

        let clients = self.inner.clients.read().await;
        Some(
            ids.iter()
                .zip(is_op.iter())
                .filter_map(|(id, _)| clients.get(id).cloned())
                .collect(),
        )
    }

    /// Get channel member nicks with operator prefix (`@`).
    pub async fn channel_nicks_with_prefix(&self, channel_name: &str) -> Option<Vec<String>> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        let channel = channels.get(&key)?;
        let ids = channel.member_list();
        let ops: Vec<bool> = ids.iter().map(|id| channel.is_operator(*id)).collect();
        drop(channels);

        let clients = self.inner.clients.read().await;
        let mut nicks = Vec::new();
        for (id, is_op) in ids.iter().zip(ops.iter()) {
            if let Some(h) = clients.get(id) {
                let prefix = if *is_op { "@" } else { "" };
                nicks.push(format!("{}{}", prefix, h.info.nick));
            }
        }
        Some(nicks)
    }

    /// Get all channels a client is a member of.
    pub async fn channels_for_client(&self, id: ClientId) -> Vec<String> {
        let channels = self.inner.channels.read().await;
        channels
            .iter()
            .filter_map(|(_, ch)| {
                if ch.is_member(id) {
                    Some(ch.name.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Forcibly disconnect a client by nickname.
    ///
    /// Finds the client, collects their peers in shared channels, removes the
    /// client from all state, and returns `(disconnected_handle, peer_handles)`.
    /// The caller is responsible for sending ERROR / QUIT messages.
    pub async fn force_disconnect(&self, nick: &str) -> Option<(ClientHandle, Vec<ClientHandle>)> {
        let handle = self.find_client_by_nick(nick).await?;
        let peers = self.peers_in_shared_channels(handle.id).await;
        self.remove_client(handle.id).await;
        Some((handle, peers))
    }

    /// Collect all unique peer handles that share at least one channel with `id`.
    pub async fn peers_in_shared_channels(&self, id: ClientId) -> Vec<ClientHandle> {
        let channels = self.inner.channels.read().await;
        let mut peer_ids = std::collections::HashSet::new();
        for ch in channels.values() {
            if ch.is_member(id) {
                for &mid in &ch.members {
                    if mid != id {
                        peer_ids.insert(mid);
                    }
                }
            }
        }
        drop(channels);

        let clients = self.inner.clients.read().await;
        peer_ids
            .iter()
            .filter_map(|pid| clients.get(pid).cloned())
            .collect()
    }

    /// Get channel member handles excluding a given client.
    pub async fn channel_members_except(
        &self,
        channel_name: &str,
        exclude: ClientId,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        let channel = channels.get(&key)?;
        let ids: Vec<ClientId> = channel
            .members
            .iter()
            .filter(|&&mid| mid != exclude)
            .copied()
            .collect();
        drop(channels);

        let clients = self.inner.clients.read().await;
        Some(
            ids.iter()
                .filter_map(|id| clients.get(id).cloned())
                .collect(),
        )
    }

    /// Check whether a client is an operator in a channel.
    pub async fn is_channel_operator(&self, channel_name: &str, id: ClientId) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels
            .get(&key)
            .is_some_and(|ch| ch.is_operator(id))
    }

    /// Grant or revoke operator status for a client in a channel.
    pub async fn set_channel_operator(
        &self,
        channel_name: &str,
        target_id: ClientId,
        grant: bool,
    ) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        if let Some(ch) = channels.get_mut(&key) {
            if !ch.is_member(target_id) {
                return false;
            }
            if grant {
                ch.operators.insert(target_id);
            } else {
                ch.operators.remove(&target_id);
            }
            true
        } else {
            false
        }
    }

    /// Remove a user from a channel (kick). Returns remaining member handles.
    pub async fn kick_from_channel(
        &self,
        channel_name: &str,
        target_id: ClientId,
    ) -> Option<Vec<ClientHandle>> {
        self.part_channel(target_id, channel_name).await
    }

    /// Set or unset a channel mode flag. Returns `true` if the channel exists.
    pub async fn set_channel_mode(
        &self,
        channel_name: &str,
        mode_char: char,
        set: bool,
        param: Option<&str>,
    ) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        if let Some(ch) = channels.get_mut(&key) {
            match mode_char {
                'i' => ch.modes.invite_only = set,
                't' => ch.modes.topic_locked = set,
                'n' => ch.modes.no_external = set,
                'k' => {
                    ch.modes.key = if set {
                        param.map(|s| s.to_string())
                    } else {
                        None
                    };
                }
                'l' => {
                    ch.modes.limit = if set {
                        param.and_then(|s| s.parse().ok())
                    } else {
                        None
                    };
                }
                _ => {}
            }
            true
        } else {
            false
        }
    }

    /// Get the mode string for a channel.
    pub async fn channel_mode_string(&self, channel_name: &str) -> Option<String> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels.get(&key).map(|ch| ch.modes.to_mode_string())
    }

    /// Pre-create default channels so they exist before anyone joins.
    pub async fn create_default_channels(&self) {
        let defaults = [
            ("#lobby", "General meeting place — agents and humans welcome"),
            ("#capabilities", "Agents announce what they can do"),
            ("#marketplace", "Post work requests and offers"),
        ];

        let mut channels = self.inner.channels.write().await;
        for (name, topic) in &defaults {
            let key = name.to_ascii_lowercase();
            channels.entry(key).or_insert_with(|| {
                let mut ch = Channel::new(name.to_string());
                ch.set_topic(
                    topic.to_string(),
                    "ChanServ".to_string(),
                    0,
                );
                ch
            });
        }
        tracing::info!("created default channels: #lobby, #capabilities, #marketplace");
    }

    // -- Shutdown -----------------------------------------------------------

    /// Notify all connected clients of server shutdown and remove them from state.
    pub async fn shutdown_all(&self) {
        let clients: Vec<ClientHandle> = self.inner.clients.read().await.values().cloned().collect();
        for client in &clients {
            let error_msg = IrcMessage {
                prefix: None,
                command: Command::Unknown("ERROR".to_string()),
                params: vec![format!(
                    "Closing Link: {} (Server shutting down)",
                    client.info.hostname
                )],
            };
            client.send_message(&error_msg);
        }
        // Clear all state.
        self.inner.clients.write().await.clear();
        self.inner.nick_to_id.write().await.clear();
        self.inner.channels.write().await.clear();
    }

    // -- HTTP API queries ---------------------------------------------------

    /// Server stats for `GET /api/stats`.
    pub async fn api_stats(&self) -> web::StatsResponse {
        let users = self.inner.clients.read().await.len();
        let channels = self.inner.channels.read().await.len();
        web::StatsResponse {
            server_name: self.inner.config.server_name.clone(),
            users_online: users as u64,
            channels_active: channels as u64,
            uptime_seconds: self.inner.started_at.elapsed().as_secs(),
        }
    }

    /// Channel listing for `GET /api/channels`.
    pub async fn api_channels(&self) -> Vec<web::ChannelInfo> {
        let channels = self.inner.channels.read().await;
        let mut result = Vec::with_capacity(channels.len());
        for ch in channels.values() {
            let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
            let modes = ch.modes.to_mode_string();

            // Look up ChanServ registration for extra metadata.
            let key = ch.name.to_ascii_lowercase();
            let reg = self.inner.services.chanserv.get_registered_channel(&key).await;
            let description = reg.as_ref().and_then(|r| r.description.clone());
            let min_reputation = reg.map(|r| r.min_reputation);

            result.push(web::ChannelInfo {
                name: ch.name.clone(),
                topic: topic_text,
                member_count: ch.member_count() as u64,
                modes,
                description,
                min_reputation,
            });
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }

    /// Reputation lookup for `GET /api/reputation/:nick`.
    pub async fn api_reputation(&self, nick: &str) -> Option<web::ReputationResponse> {
        let identity = self.inner.services.nickserv.get_identity(nick).await?;
        let auth_method = if identity.pubkey_hex.is_some() {
            "keypair"
        } else {
            "password"
        };
        Some(web::ReputationResponse {
            nick: identity.nick,
            reputation: identity.reputation,
            registered_at: identity.registered_at,
            auth_method: auth_method.to_string(),
            capabilities: identity.capabilities,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Basic IRC nickname validation — delegates to the shared protocol library.
fn is_valid_nick(nick: &str) -> bool {
    airc_shared::validate::is_valid_nick(nick)
}
