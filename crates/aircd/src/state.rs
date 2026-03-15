//! Shared server state — the single source of truth for clients and channels.
//!
//! All mutations flow through [`SharedState`] methods so the interface can be
//! extracted into a trait later for testing or alternative backends.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use thiserror::Error;
use tokio::sync::RwLock;

use airc_shared::aircd_ipc;
use airc_shared::{Command, IrcMessage};

use crate::channel::Channel;
use crate::client::{ClientHandle, ClientId, ClientInfo, ClientKind};
use crate::config::ServerConfig;
use crate::logger::ChannelLogger;
use crate::relay::Relay;
use crate::services::ServicesState;
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
    /// Global nick registry: lowercase nick → local or remote.
    nick_to_kind: RwLock<HashMap<String, ClientKind>>,
    /// Per-client away messages. Absent = not away.
    away_messages: RwLock<HashMap<ClientId, String>>,
    next_id: AtomicU64,
    config: ServerConfig,
    logger: ChannelLogger,
    relay: Arc<dyn Relay>,
    started_at: Instant,
    /// Embedded NickServ / ChanServ services. Set after SharedState is created
    /// to avoid the chicken-and-egg dependency (ServicesState needs SharedState).
    services: tokio::sync::OnceCell<Arc<ServicesState>>,
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
    /// Create a fresh server state from the given config and relay backend.
    pub fn new(config: ServerConfig, relay: Arc<dyn Relay>) -> Self {
        let log_dir = config.log_dir.as_ref().map(PathBuf::from);
        Self {
            inner: Arc::new(Inner {
                clients: RwLock::new(HashMap::new()),
                channels: RwLock::new(HashMap::new()),
                nick_to_kind: RwLock::new(HashMap::new()),
                away_messages: RwLock::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                config,
                logger: ChannelLogger::new(log_dir),
                relay,
                started_at: Instant::now(),
                services: tokio::sync::OnceCell::new(),
            }),
        }
    }

    /// Access the relay backend.
    #[allow(dead_code)] // Will be used by handler.rs in upcoming relay wiring.
    pub fn relay(&self) -> &dyn Relay {
        &*self.inner.relay
    }

    /// Initialize embedded services. Must be called once after `SharedState::new()`.
    pub fn set_services(&self, services: Arc<ServicesState>) {
        // Ignore the error if already set (shouldn't happen in normal operation).
        let _ = self.inner.services.set(services);
    }

    /// Access embedded services (NickServ / ChanServ).
    ///
    /// Returns `None` if services have not been initialized yet (between
    /// `SharedState::new()` and the `set_services()` call).
    pub fn services(&self) -> Option<Arc<ServicesState>> {
        self.inner.services.get().cloned()
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
        self.inner
            .nick_to_kind
            .write()
            .await
            .insert(nick_lower, ClientKind::Local(id));
    }

    /// Remove a client from all state (client map, nick map, all channels).
    /// Returns the removed handle if it existed.
    pub async fn remove_client(&self, id: ClientId) -> Option<ClientHandle> {
        let handle = self.inner.clients.write().await.remove(&id);
        if let Some(ref h) = handle {
            let nick_lower = h.info.nick.to_ascii_lowercase();
            self.inner.nick_to_kind.write().await.remove(&nick_lower);
        }
        // Remove from every channel (by ClientId — finds and removes the nick).
        let mut channels = self.inner.channels.write().await;
        let empty_channels: Vec<String> = channels
            .iter_mut()
            .filter_map(|(name, ch)| {
                ch.remove_member_by_id(id);
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

        // Clean up away status.
        self.inner.away_messages.write().await.remove(&id);

        handle
    }

    /// Find a client by nickname (case-insensitive). Returns a cloned handle.
    ///
    /// Only resolves local clients — remote nicks return `None` (they have no
    /// `ClientHandle`). Callers that need to route to remote nicks should check
    /// `nick_to_kind` directly via the relay layer.
    pub async fn find_client_by_nick(&self, nick: &str) -> Option<ClientHandle> {
        let nick_lower = nick.to_ascii_lowercase();
        let kind = self
            .inner
            .nick_to_kind
            .read()
            .await
            .get(&nick_lower)
            .cloned();
        match kind {
            Some(ClientKind::Local(id)) => self.inner.clients.read().await.get(&id).cloned(),
            _ => None,
        }
    }

    /// Get a client by ID. Returns a cloned handle.
    pub async fn get_client(&self, id: ClientId) -> Option<ClientHandle> {
        self.inner.clients.read().await.get(&id).cloned()
    }

    /// Attempt to change a registered client's nickname.
    ///
    /// Validates the new nick, checks for uniqueness, and updates all maps
    /// (nick registry, client info, and channel membership keys).
    pub async fn update_nick(&self, id: ClientId, new_nick: &str) -> Result<(), NickError> {
        if !is_valid_nick(new_nick) {
            return Err(NickError::Invalid);
        }
        let new_lower = new_nick.to_ascii_lowercase();

        // Check uniqueness — must not collide with another client.
        {
            let nick_map = self.inner.nick_to_kind.read().await;
            if let Some(existing) = nick_map.get(&new_lower) {
                // Allow if this is the same client (case-change only).
                if *existing != ClientKind::Local(id) {
                    return Err(NickError::InUse);
                }
            }
        }

        // Perform the swap in nick registry and client info.
        let mut clients = self.inner.clients.write().await;
        let mut nick_map = self.inner.nick_to_kind.write().await;
        if let Some(handle) = clients.get_mut(&id) {
            let old_lower = handle.info.nick.to_ascii_lowercase();
            nick_map.remove(&old_lower);
            // Replace the Arc<ClientInfo> with a new one carrying the new nick.
            let mut new_info = (*handle.info).clone();
            new_info.nick = new_nick.to_string();
            handle.info = Arc::new(new_info);
            nick_map.insert(new_lower.clone(), ClientKind::Local(id));
        }
        drop(clients);
        drop(nick_map);

        // Update channel membership keys: remove old nick, re-insert with new nick.
        let mut channels = self.inner.channels.write().await;
        for ch in channels.values_mut() {
            if let Some(kind) = ch.members.remove(&new_lower) {
                // Edge case: nick was already the new_lower (shouldn't happen
                // but be safe).
                ch.members.insert(new_lower.clone(), kind);
            } else if let Some(nick) = ch.nick_for_id(id).map(|n| n.to_string()) {
                // Remove old nick entry, re-insert under new nick.
                if let Some(kind) = ch.members.remove(&nick) {
                    ch.members.insert(new_lower.clone(), kind);
                }
                // Update operators set.
                if ch.operators.remove(&nick) {
                    ch.operators.insert(new_lower.clone());
                }
            }
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
        tx: tokio::sync::mpsc::Sender<Arc<str>>,
    ) -> Result<ClientHandle, NickError> {
        if !is_valid_nick(nick) {
            return Err(NickError::Invalid);
        }
        let nick_lower = nick.to_ascii_lowercase();

        // Check uniqueness.
        {
            let nick_map = self.inner.nick_to_kind.read().await;
            if nick_map.contains_key(&nick_lower) {
                return Err(NickError::InUse);
            }
        }

        let info = Arc::new(ClientInfo {
            nick: nick.to_string(),
            username: username.to_string(),
            realname: realname.to_string(),
            hostname: hostname.to_string(),
            registered: true,
            identified: false,
            modes: String::new(),
        });

        let server_name: Arc<str> = self.server_name().into();
        let handle = ClientHandle::new(id, info, tx, server_name);

        self.inner.clients.write().await.insert(id, handle.clone());
        self.inner
            .nick_to_kind
            .write()
            .await
            .insert(nick_lower, ClientKind::Local(id));

        Ok(handle)
    }

    // -- Channel management -------------------------------------------------

    /// Join a client to a channel. Creates the channel if it doesn't exist and
    /// makes the joiner an operator. Returns the `Channel` snapshot and list of
    /// local member handles (for broadcasting the JOIN).
    pub async fn join_channel(
        &self,
        id: ClientId,
        channel_name: &str,
    ) -> (Channel, Vec<ClientHandle>) {
        let key = channel_name.to_ascii_lowercase();

        // Resolve the client's nick for channel membership.
        let nick = {
            let clients = self.inner.clients.read().await;
            match clients.get(&id) {
                Some(h) => h.info.nick.clone(),
                None => return (Channel::new(channel_name.to_string()), vec![]),
            }
        };
        let nick_lower = nick.to_ascii_lowercase();

        let mut channels = self.inner.channels.write().await;
        let channel = channels
            .entry(key)
            .or_insert_with(|| Channel::new(channel_name.to_string()));

        let is_new_channel = channel.members.is_empty();
        channel.add_member(&nick, ClientKind::Local(id));
        if is_new_channel {
            channel.operators.insert(nick_lower);
        }

        let snapshot = channel.clone();
        drop(channels);

        // Collect handles for all local members (for broadcasting).
        let local_ids = snapshot.local_client_ids();
        let clients = self.inner.clients.read().await;
        let handles: Vec<ClientHandle> = local_ids
            .iter()
            .filter_map(|mid| clients.get(mid).cloned())
            .collect();

        (snapshot, handles)
    }

    /// Remove a client from a channel. Returns `None` if the client was not a
    /// member. Otherwise returns a snapshot of remaining local member handles.
    pub async fn part_channel(
        &self,
        id: ClientId,
        channel_name: &str,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        let channel = channels.get_mut(&key)?;

        // Remove by ClientId — returns the nick if found.
        if channel.remove_member_by_id(id).is_none() {
            return None;
        }

        let remaining_ids = channel.local_client_ids();
        let is_empty = channel.members.is_empty();

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

    /// Set the topic on a channel. Returns the local member handles for broadcasting.
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
        let local_ids = channel.local_client_ids();
        drop(channels);

        let clients = self.inner.clients.read().await;
        Some(
            local_ids
                .iter()
                .filter_map(|id| clients.get(id).cloned())
                .collect(),
        )
    }

    /// List all channel names with their member counts and topics.
    #[allow(dead_code)] // Superseded by list_channels_for() but kept for API/tests.
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

    /// Get local member handles for a channel.
    pub async fn channel_members(&self, channel_name: &str) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        let channel = channels.get(&key)?;
        let local_ids = channel.local_client_ids();
        drop(channels);

        let clients = self.inner.clients.read().await;
        Some(
            local_ids
                .iter()
                .filter_map(|id| clients.get(id).cloned())
                .collect(),
        )
    }

    /// Get channel member nicks with operator prefix (`@`).
    pub async fn channel_nicks_with_prefix(&self, channel_name: &str) -> Option<Vec<String>> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        let channel = channels.get(&key)?;
        Some(channel.nicks_with_prefix())
    }

    /// Get all channels a client is a member of.
    pub async fn channels_for_client(&self, id: ClientId) -> Vec<String> {
        let channels = self.inner.channels.read().await;
        channels
            .iter()
            .filter_map(|(_, ch)| {
                if ch.is_member_id(id) {
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

    /// Collect all unique local peer handles that share at least one channel with `id`.
    pub async fn peers_in_shared_channels(&self, id: ClientId) -> Vec<ClientHandle> {
        let channels = self.inner.channels.read().await;
        let mut peer_ids = std::collections::HashSet::new();
        for ch in channels.values() {
            if ch.is_member_id(id) {
                for &local_id in &ch.local_client_ids() {
                    if local_id != id {
                        peer_ids.insert(local_id);
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

    /// Return handles for all currently connected local clients.
    pub async fn all_clients(&self) -> Vec<ClientHandle> {
        self.inner.clients.read().await.values().cloned().collect()
    }

    /// Return `true` if `a` and `b` share at least one common channel.
    pub async fn shares_channel(&self, a: ClientId, b: ClientId) -> bool {
        let channels = self.inner.channels.read().await;
        channels
            .values()
            .any(|ch| ch.is_member_id(a) && ch.is_member_id(b))
    }

    /// Get local channel member handles excluding a given client.
    pub async fn channel_members_except(
        &self,
        channel_name: &str,
        exclude: ClientId,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        let channel = channels.get(&key)?;
        let ids: Vec<ClientId> = channel
            .local_client_ids()
            .into_iter()
            .filter(|&mid| mid != exclude)
            .collect();
        drop(channels);

        let clients = self.inner.clients.read().await;
        Some(
            ids.iter()
                .filter_map(|id| clients.get(id).cloned())
                .collect(),
        )
    }

    /// Check whether a client is an operator in a channel (by `ClientId`).
    pub async fn is_channel_operator(&self, channel_name: &str, id: ClientId) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels.get(&key).is_some_and(|ch| ch.is_operator_id(id))
    }

    /// Grant or revoke operator status for a nick in a channel.
    pub async fn set_channel_operator(
        &self,
        channel_name: &str,
        target_nick: &str,
        grant: bool,
    ) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let nick_lower = target_nick.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        if let Some(ch) = channels.get_mut(&key) {
            if !ch.is_member_nick(target_nick) {
                return false;
            }
            if grant {
                ch.operators.insert(nick_lower);
            } else {
                ch.operators.remove(&nick_lower);
            }
            true
        } else {
            false
        }
    }

    /// Remove a user from a channel by nick (kick). Returns remaining local member handles.
    pub async fn kick_from_channel(
        &self,
        channel_name: &str,
        target_nick: &str,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        let channel = channels.get_mut(&key)?;

        if !channel.remove_member(target_nick) {
            return None;
        }

        let remaining_ids = channel.local_client_ids();
        let is_empty = channel.members.is_empty();

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
                'm' => ch.modes.moderated = set,
                's' => ch.modes.secret = set,
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

    /// Get the creation timestamp for a channel.
    pub async fn channel_created_at(&self, channel_name: &str) -> Option<u64> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels.get(&key).map(|ch| ch.created_at)
    }

    /// Grant or revoke voice (+v) for a nick in a channel.
    pub async fn set_channel_voice(
        &self,
        channel_name: &str,
        target_nick: &str,
        grant: bool,
    ) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let nick_lower = target_nick.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        if let Some(ch) = channels.get_mut(&key) {
            if !ch.is_member_nick(target_nick) {
                return false;
            }
            if grant {
                ch.voiced.insert(nick_lower);
            } else {
                ch.voiced.remove(&nick_lower);
            }
            true
        } else {
            false
        }
    }

    /// Check whether a nick can speak in a channel (+m enforcement).
    /// Returns `true` if the channel is not moderated, or if the nick is op/voiced.
    pub async fn can_speak_in_channel(&self, channel_name: &str, nick: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        match channels.get(&key) {
            Some(ch) => ch.can_speak(nick),
            None => true,
        }
    }

    /// Check whether a nick is a member of a channel.
    pub async fn is_channel_member(&self, channel_name: &str, nick: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels.get(&key).is_some_and(|ch| ch.is_member_nick(nick))
    }

    /// Check whether a channel has +n (no external messages) mode set.
    pub async fn channel_is_no_external(&self, channel_name: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels.get(&key).is_some_and(|ch| ch.modes.no_external)
    }

    /// Check whether a channel is secret (+s).
    pub async fn channel_is_secret(&self, channel_name: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels.get(&key).is_some_and(|ch| ch.modes.secret)
    }

    /// Get the count of local clients.
    pub async fn local_client_count(&self) -> usize {
        self.inner.clients.read().await.len()
    }

    /// Get the count of active channels.
    pub async fn channel_count(&self) -> usize {
        self.inner.channels.read().await.len()
    }

    /// Get the count of IRC operators online.
    pub async fn oper_count(&self) -> usize {
        self.inner
            .clients
            .read()
            .await
            .values()
            .filter(|h| h.info.is_oper())
            .count()
    }

    /// List channels, filtering out secret (+s) channels for non-members.
    pub async fn list_channels_for(
        &self,
        client_id: ClientId,
    ) -> Vec<(String, usize, Option<String>)> {
        let channels = self.inner.channels.read().await;
        channels
            .values()
            .filter(|ch| {
                // Show if not secret, or if the client is a member.
                !ch.modes.secret || ch.is_member_id(client_id)
            })
            .map(|ch| {
                let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
                (ch.name.clone(), ch.member_count(), topic_text)
            })
            .collect()
    }

    /// Get channels for a client as seen by another client (for WHOIS).
    /// Filters out secret channels where the querier is not a member.
    pub async fn channels_for_client_seen_by(
        &self,
        target_id: ClientId,
        querier_id: ClientId,
    ) -> Vec<String> {
        let channels = self.inner.channels.read().await;
        channels
            .iter()
            .filter_map(|(_, ch)| {
                if ch.is_member_id(target_id) && (!ch.modes.secret || ch.is_member_id(querier_id)) {
                    Some(ch.name.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Part a client from ALL channels (for JOIN 0).
    /// Returns a list of (channel_name, remaining_local_members) for broadcasting PART.
    pub async fn part_all_channels(&self, id: ClientId) -> Vec<(String, Vec<ClientHandle>)> {
        let mut channels = self.inner.channels.write().await;

        // Collect channel names where this client is a member.
        let member_channels: Vec<String> = channels
            .iter()
            .filter_map(|(key, ch)| {
                if ch.is_member_id(id) {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();

        let mut results = Vec::new();
        for key in &member_channels {
            if let Some(channel) = channels.get_mut(key) {
                let name = channel.name.clone();
                channel.remove_member_by_id(id);
                let remaining_ids = channel.local_client_ids();
                let is_empty = channel.members.is_empty();
                if is_empty {
                    channels.remove(key);
                }
                results.push((name, remaining_ids));
            }
        }

        drop(channels);

        // Resolve ClientHandles.
        let clients = self.inner.clients.read().await;
        results
            .into_iter()
            .map(|(name, ids)| {
                let handles: Vec<ClientHandle> = ids
                    .iter()
                    .filter_map(|cid| clients.get(cid).cloned())
                    .collect();
                (name, handles)
            })
            .collect()
    }

    /// Pre-create default channels so they exist before anyone joins.
    pub async fn create_default_channels(&self) {
        let defaults = [
            (
                "#lobby",
                "General meeting place — agents and humans welcome",
            ),
            ("#capabilities", "Agents announce what they can do"),
            ("#marketplace", "Post work requests and offers"),
        ];

        let mut channels = self.inner.channels.write().await;
        for (name, topic) in &defaults {
            let key = name.to_ascii_lowercase();
            channels.entry(key).or_insert_with(|| {
                let mut ch = Channel::new(name.to_string());
                ch.set_topic(topic.to_string(), "ChanServ".to_string(), 0);
                ch
            });
        }
        tracing::info!("created default channels: #lobby, #capabilities, #marketplace");
    }

    // -- Away management ----------------------------------------------------

    /// Set a client's away message.
    pub async fn set_away(&self, id: ClientId, message: String) {
        self.inner.away_messages.write().await.insert(id, message);
    }

    /// Clear a client's away status. Returns `true` if they were away.
    pub async fn clear_away(&self, id: ClientId) -> bool {
        self.inner.away_messages.write().await.remove(&id).is_some()
    }

    /// Get a client's away message, if set.
    pub async fn get_away_message(&self, id: ClientId) -> Option<String> {
        self.inner.away_messages.read().await.get(&id).cloned()
    }

    // -- User mode management ------------------------------------------------

    /// Add a user mode flag to a client (e.g. `'o'`, `'S'`).
    pub async fn add_user_mode(&self, id: ClientId, flag: char) {
        if let Some(handle) = self.inner.clients.write().await.get_mut(&id) {
            handle.info = Arc::new(handle.info.with_mode(flag));
        }
    }

    /// Remove a user mode flag from a client (e.g. `'i'`).
    pub async fn remove_user_mode(&self, id: ClientId, flag: char) {
        if let Some(handle) = self.inner.clients.write().await.get_mut(&id) {
            handle.info = Arc::new(handle.info.without_mode(flag));
        }
    }

    /// Return `true` if the given client has the invisible (`+i`) mode set.
    #[allow(dead_code)]
    pub async fn client_is_invisible(&self, id: ClientId) -> bool {
        self.inner
            .clients
            .read()
            .await
            .get(&id)
            .is_some_and(|h| h.info.is_invisible())
    }

    /// Get the user mode string for a client (e.g. `"+oS"`).
    pub async fn user_mode_string(&self, id: ClientId) -> String {
        self.inner
            .clients
            .read()
            .await
            .get(&id)
            .map(|h| {
                if h.info.modes.is_empty() {
                    "+".to_string()
                } else {
                    format!("+{}", h.info.modes)
                }
            })
            .unwrap_or_else(|| "+".to_string())
    }

    // -- Invite management --------------------------------------------------

    /// Add a nick to a channel's invite list. Returns `true` if the channel exists.
    pub async fn add_channel_invite(&self, channel_name: &str, nick: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        if let Some(ch) = channels.get_mut(&key) {
            ch.add_invite(nick);
            true
        } else {
            false
        }
    }

    /// Check whether a nick is on a channel's invite list.
    #[allow(dead_code)] // Available for future direct-query paths.
    pub async fn is_channel_invited(&self, channel_name: &str, nick: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels.get(&key).is_some_and(|ch| ch.is_invited(nick))
    }

    /// Clear a nick from a channel's invite list (after successful join).
    pub async fn clear_channel_invite(&self, channel_name: &str, nick: &str) {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        if let Some(ch) = channels.get_mut(&key) {
            ch.clear_invite(nick);
        }
    }

    // -- Relay notifications ------------------------------------------------
    //
    // Fire-and-forget wrapper around `Relay::publish`. Errors are logged
    // but never fail the calling IRC command — relay failures are transient
    // and will self-heal on the next heartbeat / reconnect.

    /// Broadcast an IRC message to all remote nodes.
    ///
    /// Called after processing a command that mutates shared state or needs
    /// remote delivery (JOIN, PART, QUIT, NICK, PRIVMSG, etc.).
    pub async fn relay_publish(&self, message: &IrcMessage) {
        if let Err(e) = self.inner.relay.publish(message).await {
            tracing::warn!(error = %e, "relay: failed to publish message");
        }
    }

    /// Subscribe to inbound relay events. Returns the receiver for the
    /// server's select loop.
    pub async fn relay_subscribe(
        &self,
    ) -> Result<tokio::sync::mpsc::Receiver<crate::relay::InboundEvent>, crate::relay::RelayError>
    {
        self.inner.relay.subscribe().await
    }

    // -- Remote state management --------------------------------------------
    //
    // Methods used by the inbound relay task to update local state in response
    // to events from remote nodes.

    /// Register a remote nick in the global nick registry.
    ///
    /// Called when the inbound relay handler sees a NICK or JOIN from a
    /// remote node, or during `NodeUp` bulk registration.
    pub async fn add_remote_nick(&self, nick: &str, node_id: crate::client::NodeId) {
        let nick_lower = nick.to_ascii_lowercase();
        self.inner
            .nick_to_kind
            .write()
            .await
            .insert(nick_lower, ClientKind::Remote(node_id));
    }

    /// Remove a remote nick from the global nick registry.
    ///
    /// Called when the inbound relay handler sees a QUIT from a remote node.
    pub async fn remove_remote_nick(&self, nick: &str) {
        let nick_lower = nick.to_ascii_lowercase();
        let mut nick_map = self.inner.nick_to_kind.write().await;
        // Only remove if it's actually a Remote entry (don't accidentally
        // remove a local nick if there's a race).
        if let Some(kind) = nick_map.get(&nick_lower) {
            if matches!(kind, ClientKind::Remote(_)) {
                nick_map.remove(&nick_lower);
            }
        }
    }

    /// Add a remote nick to a channel's membership.
    ///
    /// Creates the channel if it doesn't exist (remote node's join means the
    /// channel exists on the network).
    pub async fn add_remote_channel_member(
        &self,
        channel_name: &str,
        nick: &str,
        node_id: crate::client::NodeId,
    ) {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        let channel = channels
            .entry(key)
            .or_insert_with(|| Channel::new(channel_name.to_string()));
        channel.add_member(nick, ClientKind::Remote(node_id));
    }

    /// Remove a remote nick from a channel's membership.
    pub async fn remove_remote_channel_member(&self, channel_name: &str, nick: &str) {
        let key = channel_name.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        if let Some(channel) = channels.get_mut(&key) {
            channel.remove_member(nick);
            if channel.members.is_empty() {
                channels.remove(&key);
            }
        }
    }

    /// Remove a remote nick from ALL channels it belongs to.
    ///
    /// Called when the inbound relay handler sees a QUIT from a remote node.
    pub async fn remove_remote_nick_from_all_channels(&self, nick: &str) {
        let nick_lower = nick.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;
        for channel in channels.values_mut() {
            channel.remove_member(&nick_lower);
        }
        // Clean up empty channels.
        channels.retain(|_, ch| !ch.members.is_empty());
    }

    /// Get all locally connected client handles.
    ///
    /// Used by the inbound relay handler to broadcast events (like NICK
    /// changes or QUITs from remote nodes) to local clients.
    pub async fn all_local_clients(&self) -> Vec<ClientHandle> {
        self.inner.clients.read().await.values().cloned().collect()
    }

    /// Remove all remote members belonging to a node from all channels and
    /// the nick registry. Returns the list of removed nicks (for QUIT broadcast).
    pub async fn remove_node(&self, node_id: &crate::client::NodeId) -> Vec<String> {
        // Remove from nick registry.
        let mut nick_map = self.inner.nick_to_kind.write().await;
        let removed_nicks: Vec<String> = nick_map
            .iter()
            .filter_map(|(nick, kind)| match kind {
                ClientKind::Remote(nid) if nid == node_id => Some(nick.clone()),
                _ => None,
            })
            .collect();
        for nick in &removed_nicks {
            nick_map.remove(nick);
        }
        drop(nick_map);

        // Remove from all channels.
        let mut channels = self.inner.channels.write().await;
        for ch in channels.values_mut() {
            ch.remove_node_members(node_id);
        }
        // Clean up empty channels.
        channels.retain(|_, ch| !ch.members.is_empty());

        removed_nicks
    }

    /// Look up the `ClientKind` for a nick (case-insensitive).
    ///
    /// Returns `None` if the nick is not registered anywhere. This is used
    /// by the DM routing logic to decide whether to deliver locally, relay
    /// to a remote node, or return ERR_NOSUCHNICK.
    pub async fn nick_kind(&self, nick: &str) -> Option<ClientKind> {
        let nick_lower = nick.to_ascii_lowercase();
        self.inner
            .nick_to_kind
            .read()
            .await
            .get(&nick_lower)
            .cloned()
    }

    /// Collect all unique local `ClientHandle`s that share at least one channel
    /// with any remote member from the given node.
    ///
    /// Used before `remove_node()` to identify local peers that need a netsplit
    /// QUIT notification.
    pub async fn local_peers_of_node(&self, node_id: &crate::client::NodeId) -> Vec<ClientHandle> {
        let channels = self.inner.channels.read().await;
        let mut local_ids = std::collections::HashSet::new();
        for ch in channels.values() {
            // Check if this channel has any members from the departing node.
            let has_node_member = ch
                .members
                .values()
                .any(|kind| matches!(kind, ClientKind::Remote(nid) if nid == node_id));
            if has_node_member {
                // Collect all local member IDs.
                for id in ch.local_client_ids() {
                    local_ids.insert(id);
                }
            }
        }
        drop(channels);

        let clients = self.inner.clients.read().await;
        local_ids
            .iter()
            .filter_map(|id| clients.get(id).cloned())
            .collect()
    }

    // -- Shutdown -----------------------------------------------------------

    /// Notify all connected clients of server shutdown and remove them from state.
    pub async fn shutdown_all(&self) {
        let clients: Vec<ClientHandle> =
            self.inner.clients.read().await.values().cloned().collect();
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
        self.inner.nick_to_kind.write().await.clear();
        self.inner.channels.write().await.clear();
        self.inner.away_messages.write().await.clear();
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

            result.push(web::ChannelInfo {
                name: ch.name.clone(),
                topic: topic_text,
                member_count: ch.member_count() as u64,
                modes,
                // TODO(phase2): Populate from ChanServ via service protocol extensions.
                description: None,
                min_reputation: None,
            });
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }

    // -- IPC queries --------------------------------------------------------

    /// Full server stats for IPC (`aircd status`) and Prometheus metrics.
    pub async fn stats(&self) -> aircd_ipc::StatsResponse {
        let users = self.inner.clients.read().await.len();
        let channels = self.inner.channels.read().await;
        let channels_active = channels.len() as u64;

        let mut channel_list = Vec::with_capacity(channels.len());
        for ch in channels.values() {
            let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
            let modes = ch.modes.to_mode_string();

            channel_list.push(aircd_ipc::ChannelInfo {
                name: ch.name.clone(),
                topic: topic_text,
                member_count: ch.member_count() as u64,
                modes,
                // TODO(phase2): Populate from ChanServ via service protocol extensions.
                description: None,
                min_reputation: None,
            });
        }
        drop(channels);

        channel_list.sort_by(|a, b| a.name.cmp(&b.name));

        aircd_ipc::StatsResponse {
            server_name: self.inner.config.server_name.clone(),
            users_online: users as u64,
            channels_active,
            uptime_seconds: self.inner.started_at.elapsed().as_secs(),
            channels: channel_list,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Basic IRC nickname validation — delegates to the shared protocol library.
fn is_valid_nick(nick: &str) -> bool {
    airc_shared::validate::is_valid_nick(nick)
}
