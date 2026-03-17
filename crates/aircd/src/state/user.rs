//! User registry: add, remove, look up, rename, and iterate clients.
//!
//! Also covers: away status, user mode flags, shutdown, WHO matching,
//! and the `force_disconnect` / `peers_in_shared_channels` helpers.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use airc_shared::{Command, IrcMessage};

use crate::channel::Channel;
use crate::client::{Client, ClientHandle, ClientId, ClientInfo, ClientKind, NodeId};

use super::{NUM_SHARDS, NickError, SharedState, is_valid_nick};

impl SharedState {
    /// Register a fully-connected local client in the shared state.
    #[allow(dead_code)]
    pub async fn add_client(&self, client: Client) {
        let nick_lower = client.info.nick.to_ascii_lowercase();
        let id = client.id;
        self.user_shard(id).write().await.insert(id, client);
        self.inner.nick_index.write().await.insert(nick_lower, id);
    }

    /// Remove a local client from all state (user map, all channels, away).
    /// Returns the removed client if it existed.
    pub async fn remove_client(&self, id: ClientId) -> Option<ClientHandle> {
        let client = self.user_shard(id).write().await.remove(&id);

        // Remove from nick index.
        if let Some(ref c) = client {
            self.inner
                .nick_index
                .write()
                .await
                .remove(&c.info.nick.to_ascii_lowercase());
        }

        // Remove from membership_index and capture the client's channel set so we
        // only touch the channels this client was actually in (O(channels_for_client)
        // instead of O(all_channels)).
        let client_keys: HashSet<String> = self
            .inner
            .membership_index
            .write()
            .await
            .remove(&id)
            .unwrap_or_default();

        // Collect arcs only for channels the client was in (brief map read).
        let channel_arcs: Vec<(String, Arc<RwLock<Channel>>)> = {
            let map = self.inner.channels.read().await;
            client_keys
                .iter()
                .filter_map(|key| map.get(key).map(|arc| (key.clone(), arc.clone())))
                .collect()
        };

        // Remove from each channel; collect keys of channels that became empty.
        let mut empty_keys: Vec<String> = Vec::new();
        for (key, arc) in channel_arcs {
            let mut ch = arc.write().await;
            ch.remove_member_by_id(id);
            if ch.members.is_empty() {
                empty_keys.push(key);
            }
        }

        // Remove empty channels from the map.
        if !empty_keys.is_empty() {
            let mut map = self.inner.channels.write().await;
            for key in &empty_keys {
                if let Some(arc) = map.get(key) {
                    if arc.read().await.members.is_empty() {
                        map.remove(key);
                    }
                }
            }
        }

        client
    }

    /// Find a local client by nickname (case-insensitive O(1) index lookup).
    ///
    /// Returns `None` if the nick belongs to a remote client or does not exist.
    pub async fn find_client_by_nick(&self, nick: &str) -> Option<ClientHandle> {
        let nick_lower = nick.to_ascii_lowercase();
        let index = self.inner.nick_index.read().await;
        let id = *index.get(&nick_lower)?;
        drop(index);
        let shard = self.user_shard(id).read().await;
        let client = shard.get(&id)?;
        if client.is_local() {
            Some(client.clone())
        } else {
            None
        }
    }

    /// Find any client (local or remote) by nickname (case-insensitive O(1) index lookup).
    pub async fn find_user_by_nick(&self, nick: &str) -> Option<Client> {
        let nick_lower = nick.to_ascii_lowercase();
        let id = *self.inner.nick_index.read().await.get(&nick_lower)?;
        self.user_shard(id).read().await.get(&id).cloned()
    }

    /// Get a client by ID. Returns a cloned handle.
    pub async fn get_client(&self, id: ClientId) -> Option<ClientHandle> {
        self.user_shard(id).read().await.get(&id).cloned()
    }

    /// Attempt to change a registered client's nickname.
    ///
    /// Validates the new nick, checks for uniqueness, and updates the user map
    /// and channel membership keys.
    pub async fn update_nick(&self, id: ClientId, new_nick: &str) -> Result<(), NickError> {
        if !is_valid_nick(new_nick) {
            return Err(NickError::Invalid);
        }
        let new_lower = new_nick.to_ascii_lowercase();

        // Lock nick_index first (ordering: nick_index before user shard).
        let mut nick_index = self.inner.nick_index.write().await;

        // Check uniqueness via index (O(1)).
        if let Some(&existing_id) = nick_index.get(&new_lower) {
            if existing_id != id {
                return Err(NickError::InUse);
            }
        }

        // Now acquire the shard for this client.
        let mut shard = self.user_shard(id).write().await;

        // Update user info and index.
        if let Some(client) = shard.get_mut(&id) {
            let old_lower = client.info.nick.to_ascii_lowercase();
            let mut new_info = (*client.info).clone();
            new_info.nick = new_nick.to_string();
            client.info = Arc::new(new_info);
            nick_index.remove(&old_lower);
            nick_index.insert(new_lower, id);
        }

        // Update channel membership keys: channels now store ClientId directly,
        // so no renaming is needed — only operator/voice sets track nicks and
        // those are updated here.
        // NOTE: After Step 5 channels use HashSet<ClientId> so no update needed.

        Ok(())
    }

    /// Check nick availability, reserve it, and mark the client as registered.
    ///
    /// Creates the `Client`, inserts it into state, and returns a clone.
    ///
    /// `account` is the NickServ account name if the client authenticated via
    /// SASL before completing registration, `None` otherwise.
    pub async fn register_client(
        &self,
        id: ClientId,
        nick: &str,
        username: &str,
        realname: &str,
        hostname: &str,
        tx: tokio::sync::mpsc::Sender<Arc<str>>,
        cancel: CancellationToken,
        account: Option<String>,
    ) -> Result<ClientHandle, NickError> {
        if !is_valid_nick(nick) {
            return Err(NickError::Invalid);
        }
        let nick_lower = nick.to_ascii_lowercase();

        let identified = account.is_some();
        let info = Arc::new(ClientInfo {
            nick: nick.to_string(),
            username: username.to_string(),
            realname: realname.to_string(),
            hostname: hostname.to_string(),
            registered: true,
            identified,
            account,
            modes: 0,
            away: None,
        });

        let server_name: Arc<str> = self.server_name().into();
        let client = Client::new_local(id, info, tx, cancel, server_name);

        // Lock nick_index first, then the shard — consistent ordering.
        {
            let mut nick_index = self.inner.nick_index.write().await;
            if nick_index.contains_key(&nick_lower) {
                return Err(NickError::InUse);
            }
            let mut shard = self.user_shard(id).write().await;
            shard.insert(id, client.clone());
            nick_index.insert(nick_lower, id);
        }

        Ok(client)
    }

    // -- Remote state management --------------------------------------------

    /// Register a remote client in the user map.
    ///
    /// Called when the inbound relay handler sees a `ClientIntro` from a
    /// remote node.
    pub async fn add_remote_client(&self, client: Client) {
        let nick_lower = client.info.nick.to_ascii_lowercase();
        let id = client.id;
        self.user_shard(id).write().await.insert(id, client);
        self.inner.nick_index.write().await.insert(nick_lower, id);
    }

    /// Remove a remote client from the user map (and all channels).
    ///
    /// Called when the inbound relay handler sees a `ClientDown` or a
    /// `NodeDown` cleanup.
    pub async fn remove_remote_client(&self, id: ClientId) {
        // Capture the nick before removing from users map.
        let nick_lower = {
            let shard = self.user_shard(id).read().await;
            shard.get(&id).map(|c| c.info.nick.to_ascii_lowercase())
        };
        self.user_shard(id).write().await.remove(&id);
        if let Some(n) = nick_lower {
            self.inner.nick_index.write().await.remove(&n);
        }

        // Remove from membership_index and capture the client's channel set so we
        // only touch the channels this client was actually in.
        let client_keys: HashSet<String> = self
            .inner
            .membership_index
            .write()
            .await
            .remove(&id)
            .unwrap_or_default();

        // Collect arcs only for channels the client was in (brief map read).
        let channel_arcs: Vec<(String, Arc<RwLock<Channel>>)> = {
            let map = self.inner.channels.read().await;
            client_keys
                .iter()
                .filter_map(|key| map.get(key).map(|arc| (key.clone(), arc.clone())))
                .collect()
        };

        let mut empty_keys = Vec::new();
        for (key, arc) in channel_arcs {
            let mut ch = arc.write().await;
            ch.remove_member_by_id(id);
            if ch.members.is_empty() {
                empty_keys.push(key);
            }
        }

        if !empty_keys.is_empty() {
            let mut map = self.inner.channels.write().await;
            for key in &empty_keys {
                if let Some(arc) = map.get(key) {
                    if arc.read().await.members.is_empty() {
                        map.remove(key);
                    }
                }
            }
        }
    }

    /// Update a remote client's nick in the user map.
    ///
    /// Called when the inbound relay handler sees a `NickChange` from a
    /// remote node.  The `ClientId` stays the same; only the info is updated.
    pub async fn update_client_nick(&self, id: ClientId, new_nick: &str) {
        // Lock nick_index first, then the shard — consistent ordering.
        let mut nick_index = self.inner.nick_index.write().await;
        let mut shard = self.user_shard(id).write().await;
        if let Some(client) = shard.get_mut(&id) {
            let old_lower = client.info.nick.to_ascii_lowercase();
            let new_lower = new_nick.to_ascii_lowercase();
            let mut new_info = (*client.info).clone();
            new_info.nick = new_nick.to_string();
            client.info = Arc::new(new_info);
            nick_index.remove(&old_lower);
            nick_index.insert(new_lower, id);
        }
    }

    /// Remove all clients belonging to a node.
    /// Returns `(removed_client_ids, nicks)` for QUIT broadcast.
    pub async fn remove_node(&self, node_id: &NodeId) -> Vec<Client> {
        // Collect removed clients across all shards.
        let mut removed: Vec<Client> = Vec::new();
        for shard_lock in &self.inner.users {
            let mut shard = shard_lock.write().await;
            let node_clients: Vec<Client> = shard
                .values()
                .filter(
                    |c| matches!(&c.kind, ClientKind::Remote { node_id: nid } if nid == node_id),
                )
                .cloned()
                .collect();
            for c in &node_clients {
                shard.remove(&c.id);
            }
            removed.extend(node_clients);
        }

        // Batch-remove from nick_index.
        {
            let mut nick_index = self.inner.nick_index.write().await;
            for c in &removed {
                nick_index.remove(&c.info.nick.to_ascii_lowercase());
            }
        }

        // Batch-remove from membership_index index.
        {
            let mut cc = self.inner.membership_index.write().await;
            for c in &removed {
                cc.remove(&c.id);
            }
        }

        // Remove all those IDs from channels.
        let ids: Vec<ClientId> = removed.iter().map(|c| c.id).collect();
        let channel_arcs: Vec<(String, Arc<RwLock<Channel>>)> = self
            .inner
            .channels
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut empty_keys = Vec::new();
        for (key, arc) in channel_arcs {
            let mut ch = arc.write().await;
            for &id in &ids {
                ch.remove_member_by_id(id); // returns bool; ignore
            }
            if ch.members.is_empty() {
                empty_keys.push(key);
            }
        }

        if !empty_keys.is_empty() {
            let mut map = self.inner.channels.write().await;
            for key in &empty_keys {
                if let Some(arc) = map.get(key) {
                    if arc.read().await.members.is_empty() {
                        map.remove(key);
                    }
                }
            }
        }

        removed
    }

    /// Collect all unique local `ClientHandle`s that share at least one channel
    /// with any remote member from the given node.
    ///
    /// Used before `remove_node()` to identify local peers that need a netsplit
    /// QUIT notification.
    pub async fn local_peers_of_node(&self, node_id: &NodeId) -> Vec<ClientHandle> {
        // Collect the node's ClientIds first so we can check channel membership
        // without holding both the users lock and a channel lock simultaneously.
        let mut node_ids: HashSet<ClientId> = HashSet::new();
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            for c in shard.values() {
                if matches!(&c.kind, ClientKind::Remote { node_id: nid } if nid == node_id) {
                    node_ids.insert(c.id);
                }
            }
        }

        if node_ids.is_empty() {
            return vec![];
        }

        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();

        // For every channel that contains at least one member of the node,
        // collect all member IDs (local filter applied below via users map).
        let mut candidate_ids = HashSet::new();
        for arc in arcs {
            let ch = arc.read().await;
            let has_node_member = ch.members.keys().any(|id| node_ids.contains(id));
            if has_node_member {
                for &mid in ch.members.keys() {
                    candidate_ids.insert(mid);
                }
            }
        }

        // Group candidates by shard and look them up.
        let mut by_shard: [Vec<ClientId>; NUM_SHARDS] = std::array::from_fn(|_| Vec::new());
        for id in &candidate_ids {
            by_shard[(id.0 % NUM_SHARDS as u64) as usize].push(*id);
        }
        let mut handles = Vec::new();
        for (i, ids) in by_shard.iter().enumerate() {
            if ids.is_empty() {
                continue;
            }
            let shard = self.inner.users[i].read().await;
            for id in ids {
                if let Some(c) = shard.get(id) {
                    if c.is_local() {
                        handles.push(c.clone());
                    }
                }
            }
        }
        handles
    }

    // -- Away management ----------------------------------------------------

    /// Set or clear the away message for a client.
    /// Pass `Some(message)` to mark away, `None` to clear.
    pub async fn set_away(&self, id: ClientId, message: Option<String>) {
        if let Some(client) = self.user_shard(id).write().await.get_mut(&id) {
            let mut new_info = (*client.info).clone();
            new_info.away = message;
            client.info = Arc::new(new_info);
        }
    }

    // -- User mode management -----------------------------------------------

    /// Add a user mode flag to a client (e.g. `'o'`, `'S'`).
    pub async fn add_user_mode(&self, id: ClientId, flag: char) {
        if let Some(client) = self.user_shard(id).write().await.get_mut(&id) {
            client.info = Arc::new(client.info.with_mode(flag));
        }
    }

    /// Remove a user mode flag from a client (e.g. `'i'`).
    pub async fn remove_user_mode(&self, id: ClientId, flag: char) {
        if let Some(client) = self.user_shard(id).write().await.get_mut(&id) {
            client.info = Arc::new(client.info.without_mode(flag));
        }
    }

    /// Return `true` if the given client has the invisible (`+i`) mode set.
    #[allow(dead_code)]
    pub async fn client_is_invisible(&self, id: ClientId) -> bool {
        self.user_shard(id)
            .read()
            .await
            .get(&id)
            .is_some_and(|c| c.info.is_invisible())
    }

    /// Get the user mode string for a client (e.g. `"+oS"`).
    pub async fn user_mode_string(&self, id: ClientId) -> String {
        self.user_shard(id)
            .read()
            .await
            .get(&id)
            .map(|c| {
                let s = crate::client::user_mode::bits_to_string(c.info.modes);
                if s.is_empty() {
                    "+".to_string()
                } else {
                    format!("+{s}")
                }
            })
            .unwrap_or_else(|| "+".to_string())
    }

    // -- Disconnect / shutdown helpers ---------------------------------------

    /// Forcibly disconnect a client by nickname.
    ///
    /// Finds the client, collects their peers in shared channels, removes the
    /// client from all state, and returns `(disconnected_handle, peer_handles)`.
    pub async fn force_disconnect(&self, nick: &str) -> Option<(ClientHandle, Vec<ClientHandle>)> {
        let handle = self.find_client_by_nick(nick).await?;
        let peers = self.peers_in_shared_channels(handle.id).await;
        self.remove_client(handle.id).await;
        Some((handle, peers))
    }

    /// Collect all unique local peer handles that share at least one channel with `id`.
    pub async fn peers_in_shared_channels(&self, id: ClientId) -> Vec<ClientHandle> {
        // Get the set of channels this client is in — O(1)
        let my_keys: Vec<String> = self
            .inner
            .membership_index
            .read()
            .await
            .get(&id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        if my_keys.is_empty() {
            return vec![];
        }

        // For each of those channels, collect all member IDs
        let mut peer_ids = HashSet::new();
        {
            let channels = self.inner.channels.read().await;
            for key in &my_keys {
                if let Some(arc) = channels.get(key) {
                    let ch = arc.read().await;
                    for &mid in ch.members.keys() {
                        if mid != id {
                            peer_ids.insert(mid);
                        }
                    }
                }
            }
        }

        // Group by shard for efficient lookup.
        let mut by_shard: [Vec<ClientId>; NUM_SHARDS] = std::array::from_fn(|_| Vec::new());
        for pid in &peer_ids {
            by_shard[(pid.0 % NUM_SHARDS as u64) as usize].push(*pid);
        }
        let mut handles = Vec::new();
        for (i, ids) in by_shard.iter().enumerate() {
            if ids.is_empty() {
                continue;
            }
            let shard = self.inner.users[i].read().await;
            for pid in ids {
                if let Some(c) = shard.get(pid) {
                    if c.is_local() {
                        handles.push(c.clone());
                    }
                }
            }
        }
        handles
    }

    /// Return handles for all currently connected local clients.
    pub async fn all_clients(&self) -> Vec<ClientHandle> {
        let mut result = Vec::new();
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            for c in shard.values() {
                if c.is_local() {
                    result.push(c.clone());
                }
            }
        }
        result
    }

    /// Return all local client handles.
    ///
    /// Used by the inbound relay handler to broadcast events (like NICK
    /// changes or QUITs from remote nodes) to local clients.
    pub async fn all_local_clients(&self) -> Vec<ClientHandle> {
        let mut result = Vec::new();
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            for c in shard.values() {
                if c.is_local() {
                    result.push(c.clone());
                }
            }
        }
        result
    }

    /// Return `true` if `a` and `b` share at least one common channel.
    #[allow(dead_code)]
    pub async fn shares_channel(&self, a: ClientId, b: ClientId) -> bool {
        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();
        for arc in arcs {
            let ch = arc.read().await;
            if ch.is_member(a) && ch.is_member(b) {
                return true;
            }
        }
        false
    }

    /// Return the set of `ClientId`s that share at least one channel with the
    /// given client.
    pub async fn co_members(&self, id: ClientId) -> HashSet<ClientId> {
        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();
        let mut set = HashSet::new();
        for arc in arcs {
            let ch = arc.read().await;
            if ch.is_member(id) {
                for member_id in ch.members.keys().copied() {
                    set.insert(member_id);
                }
            }
        }
        set
    }

    /// Get the count of local clients.
    pub async fn local_client_count(&self) -> usize {
        let mut count = 0usize;
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            count += shard.values().filter(|c| c.is_local()).count();
        }
        count
    }

    /// Get the count of IRC operators online.
    pub async fn oper_count(&self) -> usize {
        let mut count = 0usize;
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            count += shard
                .values()
                .filter(|c| c.is_local() && c.info.is_oper())
                .count();
        }
        count
    }

    /// Return `(local_user_count, channel_count, oper_count)` in two passes
    /// instead of three: one pass over all user shards (collecting both user
    /// and oper counts simultaneously) and one read of the channels map length.
    ///
    /// Replaces the three independent `local_client_count` / `channel_count` /
    /// `oper_count` calls in LUSERS and similar places (LOCK-12 / ON-3).
    pub async fn server_counts(&self) -> (usize, usize, usize) {
        let mut local_users = 0usize;
        let mut opers = 0usize;
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            for c in shard.values() {
                if c.is_local() {
                    local_users += 1;
                    if c.info.is_oper() {
                        opers += 1;
                    }
                }
            }
        }
        let channels = self.inner.channels.read().await.len();
        (local_users, channels, opers)
    }

    /// Return local client handles matching a WHO mask, with visibility applied.
    ///
    /// Used by the non-channel WHO path.  Avoids the O(N) full-clone of
    /// `all_clients()` when the mask or visibility filter eliminates many entries:
    ///
    /// 1. First pass (channels): build the set of co-members (clients sharing a
    ///    channel with `requester_id`) under per-channel read locks — same
    ///    semantics as the old `co_members()` helper.
    /// 2. Second pass (user shards): iterate each shard under its read lock,
    ///    applying the visibility and mask filters in-place, cloning only the
    ///    handles that will actually be returned.
    ///
    /// Lock ordering: channels map read → per-channel read → per-shard user read.
    /// No channel lock is held while user shards are read.
    pub async fn who_matching_clients(
        &self,
        requester_id: ClientId,
        mask: &str,
    ) -> Vec<ClientHandle> {
        // --- Pass 1: build co-member set -----------------------------------------
        let channel_arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();
        let mut co_members: HashSet<ClientId> = HashSet::new();
        for arc in &channel_arcs {
            let ch = arc.read().await;
            if ch.is_member(requester_id) {
                for &member_id in ch.members.keys() {
                    co_members.insert(member_id);
                }
            }
        }
        drop(channel_arcs); // release Arc refs (and thus channel read locks)

        // --- Pass 2: iterate user shards with visibility + mask filter -----------
        let mask_is_wildcard = mask == "*";
        let mut result = Vec::new();
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            for c in shard.values() {
                if !c.is_local() {
                    continue;
                }
                // Visibility: +i users are hidden unless they are the requester
                // or share a channel with the requester.
                let skip_invisible =
                    c.info.is_invisible() && c.id != requester_id && !co_members.contains(&c.id);
                if skip_invisible {
                    continue;
                }
                // Mask: "*" matches everyone; otherwise match nick (case-insensitive).
                if !mask_is_wildcard && !c.info.nick.eq_ignore_ascii_case(mask) {
                    continue;
                }
                result.push(c.clone());
            }
        }
        result
    }

    /// Notify all connected clients of server shutdown and remove them from state.
    pub async fn shutdown_all(&self) {
        // Collect all local handles from all shards.
        let mut clients: Vec<ClientHandle> = Vec::new();
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            for c in shard.values() {
                if c.is_local() {
                    clients.push(c.clone());
                }
            }
        }
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
        // Clear all shards.
        for shard_lock in &self.inner.users {
            shard_lock.write().await.clear();
        }
        self.inner.nick_index.write().await.clear();
        self.inner.membership_index.write().await.clear();
        self.inner.channels.write().await.clear();
    }
}
