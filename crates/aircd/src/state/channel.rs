//! Channel management: lifecycle, mutations, queries, and invite handling.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::channel::{Channel, ChannelView, MemberMode};
use crate::client::{ClientHandle, ClientId};

use super::{ChannelSendResult, NUM_SHARDS, SharedState};

impl SharedState {
    // -- Private helper -----------------------------------------------------

    /// Given a slice of `ClientId`s, collect local handles grouped by shard.
    ///
    /// This is the standard fan-out pattern: group IDs by shard, lock each
    /// shard once, look up all IDs in that shard, then release.
    pub(super) async fn local_handles_for_ids(&self, ids: &[ClientId]) -> Vec<ClientHandle> {
        if ids.is_empty() {
            return vec![];
        }
        let mut by_shard: [Vec<ClientId>; NUM_SHARDS] = std::array::from_fn(|_| Vec::new());
        for id in ids {
            by_shard[(id.0 % NUM_SHARDS as u64) as usize].push(*id);
        }
        let mut handles = Vec::with_capacity(ids.len());
        for (i, shard_ids) in by_shard.iter().enumerate() {
            if shard_ids.is_empty() {
                continue;
            }
            let shard = self.inner.users[i].read().await;
            for id in shard_ids {
                if let Some(c) = shard.get(id)
                    && c.is_local() {
                        handles.push(c.clone());
                    }
            }
        }
        handles
    }

    // -- Channel lifecycle --------------------------------------------------

    /// Join a client to a channel. Creates the channel if it doesn't exist and
    /// makes the joiner an operator. Returns the `Channel` snapshot and list of
    /// local member handles (for broadcasting the JOIN).
    pub async fn join_channel(
        &self,
        id: ClientId,
        channel_name: &str,
    ) -> (Channel, Vec<ClientHandle>) {
        let key = channel_name.to_ascii_lowercase();

        // Verify the client exists.
        if self.user_shard(id).read().await.get(&id).is_none() {
            return (Channel::new(channel_name.to_string()), vec![]);
        }

        // Get or create the channel Arc under a brief write lock.
        let ch_arc = {
            let mut map = self.inner.channels.write().await;
            map.entry(key.clone())
                .or_insert_with(|| Arc::new(RwLock::new(Channel::new(channel_name.to_string()))))
                .clone()
        };

        // Now hold only the per-channel write lock.
        let snapshot = {
            let mut ch = ch_arc.write().await;
            let is_new_channel = ch.members.is_empty();
            ch.add_member(id);
            if is_new_channel {
                ch.set_operator(id, true);
            }
            ch.clone()
        };

        // Collect handles for all local members (for broadcasting) — group by shard.
        let member_ids = snapshot.all_member_ids();
        let handles = self.local_handles_for_ids(&member_ids).await;

        self.inner
            .membership_index
            .write()
            .await
            .entry(id)
            .or_default()
            .insert(key.clone());

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

        let ch_arc = self.inner.channels.read().await.get(&key)?.clone();

        let (remaining_ids, is_empty) = {
            let mut ch = ch_arc.write().await;
            if !ch.remove_member_by_id(id) {
                return None;
            }
            let remaining = ch.all_member_ids();
            let empty = ch.members.is_empty();
            (remaining, empty)
        };

        self.inner
            .membership_index
            .write()
            .await
            .entry(id)
            .or_default()
            .remove(&key);

        if is_empty {
            let mut map = self.inner.channels.write().await;
            if let Some(arc) = map.get(&key)
                && arc.read().await.members.is_empty() {
                    map.remove(&key);
                }
            return Some(vec![]);
        }

        Some(self.local_handles_for_ids(&remaining_ids).await)
    }

    /// Part a client from ALL channels (for JOIN 0).
    pub async fn part_all_channels(&self, id: ClientId) -> Vec<(String, Vec<ClientHandle>)> {
        // Get the client's channel set from the index
        let client_keys: Vec<String> = self
            .inner
            .membership_index
            .write()
            .await
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .collect();

        let member_arcs: Vec<(String, Arc<RwLock<Channel>>)> = {
            let map = self.inner.channels.read().await;
            client_keys
                .iter()
                .filter_map(|key| map.get(key).map(|arc| (key.clone(), arc.clone())))
                .collect()
        };

        let mut empty_keys: Vec<String> = Vec::new();
        let mut results: Vec<(String, Vec<ClientId>)> = Vec::new();

        for (key, arc) in &member_arcs {
            let (name, remaining_ids, is_empty) = {
                let mut ch = arc.write().await;
                let name = ch.name.clone();
                ch.remove_member_by_id(id); // returns bool; ignore
                let remaining = ch.all_member_ids();
                let empty = ch.members.is_empty();
                (name, remaining, empty)
            };
            if is_empty {
                empty_keys.push(key.clone());
            }
            results.push((name, remaining_ids));
        }

        if !empty_keys.is_empty() {
            let mut map = self.inner.channels.write().await;
            for key in &empty_keys {
                if let Some(arc) = map.get(key)
                    && arc.read().await.members.is_empty() {
                        map.remove(key);
                    }
            }
        }

        let mut output = Vec::with_capacity(results.len());
        for (name, ids) in results {
            let handles = self.local_handles_for_ids(&ids).await;
            output.push((name, handles));
        }
        output
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

        let mut map = self.inner.channels.write().await;
        for (name, topic) in &defaults {
            let key = name.to_ascii_lowercase();
            map.entry(key).or_insert_with(|| {
                let mut ch = Channel::new(name.to_string());
                ch.set_topic(topic.to_string(), "ChanServ".to_string(), 0);
                Arc::new(RwLock::new(ch))
            });
        }
        tracing::info!("created default channels: #lobby, #capabilities, #marketplace");
    }

    // -- Channel queries ----------------------------------------------------

    /// Get a snapshot of a channel (if it exists).
    pub async fn get_channel(&self, channel_name: &str) -> Option<Channel> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        Some(arc.read().await.clone())
    }

    /// Lightweight alternative to `get_channel` — returns a `ChannelView`
    /// (modes, topic, invites, member count) without cloning the `members`
    /// HashMap.  Prefer this over `get_channel` whenever per-member queries
    /// (`is_member`, `is_operator`) are not needed from the snapshot.
    pub async fn get_channel_view(&self, channel_name: &str) -> Option<ChannelView> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        Some(ChannelView::from_channel(&*arc.read().await))
    }

    /// Return `true` if `client_id` is a member of `channel_name`.
    /// Acquires the channel read lock briefly; does not clone `members`.
    pub async fn is_channel_member_id(&self, channel_name: &str, id: ClientId) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let Some(arc) = self.inner.channels.read().await.get(&key).cloned() else {
            return false;
        };
        arc.read().await.is_member(id)
    }

    /// Return `true` if `client_id` is an operator of `channel_name`.
    pub async fn is_channel_operator_id(&self, channel_name: &str, id: ClientId) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let Some(arc) = self.inner.channels.read().await.get(&key).cloned() else {
            return false;
        };
        arc.read().await.is_operator(id)
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
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        let member_ids = {
            let mut ch = arc.write().await;
            ch.set_topic(text, setter, timestamp);
            ch.all_member_ids()
        };

        Some(self.local_handles_for_ids(&member_ids).await)
    }

    /// List all channel names with their member counts and topics.
    #[allow(dead_code)]
    pub async fn list_channels(&self) -> Vec<(String, usize, Option<String>)> {
        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();
        let mut result = Vec::with_capacity(arcs.len());
        for arc in arcs {
            let ch = arc.read().await;
            let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
            result.push((ch.name.clone(), ch.member_count(), topic_text));
        }
        result
    }

    /// Get local member handles for a channel.
    pub async fn channel_members(&self, channel_name: &str) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        let member_ids = arc.read().await.all_member_ids();
        Some(self.local_handles_for_ids(&member_ids).await)
    }

    /// Return local channel members together with their `MemberMode`, for the
    /// WHO channel path.  Avoids the full `Channel::clone()` that
    /// `get_channel()` + `channel_members()` would require.
    pub async fn channel_members_with_mode(
        &self,
        channel_name: &str,
    ) -> Option<Vec<(ClientHandle, MemberMode)>> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        // Collect (ClientId, MemberMode) pairs under the channel lock, then
        // release it before acquiring user shards.
        let pairs: Vec<(ClientId, MemberMode)> = {
            let ch = arc.read().await;
            ch.members.values().map(|m| (m.client_id, m.mode)).collect()
        };
        let ids: Vec<ClientId> = pairs.iter().map(|(id, _)| *id).collect();
        let handles = self.local_handles_for_ids(&ids).await;
        // Re-join handles with modes using a HashMap for O(1) lookup.
        let mode_map: std::collections::HashMap<ClientId, MemberMode> = pairs.into_iter().collect();
        let result = handles
            .into_iter()
            .filter_map(|h| mode_map.get(&h.id).map(|&mode| (h, mode)))
            .collect();
        Some(result)
    }

    /// Get channel member nicks with operator prefix (`@`), respecting
    /// the `multi_prefix` flag.  When `multi_prefix` is `true`, all applicable
    /// prefix characters are concatenated in highest-to-lowest order (e.g.
    /// `@+nick` for an op who is also voiced); when `false`, only the highest
    /// privilege prefix is used (current behaviour).
    pub async fn channel_nicks_with_prefix_multi(
        &self,
        channel_name: &str,
        multi_prefix: bool,
    ) -> Option<Vec<String>> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();

        // Collect (client_id, mode_prefix) while holding only the channel lock.
        let members: Vec<(ClientId, &'static str)> = {
            let ch = arc.read().await;
            ch.members
                .values()
                .map(|m| {
                    let pfx = if multi_prefix {
                        m.mode.multi_prefix()
                    } else {
                        m.mode.prefix()
                    };
                    (m.client_id, pfx)
                })
                .collect()
        };

        // Group by shard for efficient lookup.
        let mut by_shard: [Vec<(ClientId, &'static str)>; NUM_SHARDS] =
            std::array::from_fn(|_| Vec::new());
        for (id, prefix) in &members {
            by_shard[(id.0 % NUM_SHARDS as u64) as usize].push((*id, *prefix));
        }

        let mut nicks: Vec<String> = Vec::with_capacity(members.len());
        for (i, entries) in by_shard.iter().enumerate() {
            if entries.is_empty() {
                continue;
            }
            let shard = self.inner.users[i].read().await;
            for (id, prefix) in entries {
                if let Some(c) = shard.get(id) {
                    nicks.push(format!("{}{}", prefix, c.info.nick));
                }
            }
        }
        Some(nicks)
    }

    /// Get channel member nicks with operator prefix (`@`).
    #[allow(dead_code)]
    pub async fn channel_nicks_with_prefix(&self, channel_name: &str) -> Option<Vec<String>> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();

        // Collect (client_id, mode_prefix) while holding only the channel lock,
        // then release it before acquiring the users lock to avoid holding both
        // locks simultaneously.
        let members: Vec<(ClientId, &'static str)> = {
            let ch = arc.read().await;
            ch.members
                .values()
                .map(|m| (m.client_id, m.mode.prefix()))
                .collect()
        };

        // Group by shard for efficient lookup.
        let mut by_shard: [Vec<(ClientId, &'static str)>; NUM_SHARDS] =
            std::array::from_fn(|_| Vec::new());
        for (id, prefix) in &members {
            by_shard[(id.0 % NUM_SHARDS as u64) as usize].push((*id, *prefix));
        }

        let mut nicks: Vec<String> = Vec::with_capacity(members.len());
        for (i, entries) in by_shard.iter().enumerate() {
            if entries.is_empty() {
                continue;
            }
            let shard = self.inner.users[i].read().await;
            for (id, prefix) in entries {
                if let Some(c) = shard.get(id) {
                    nicks.push(format!("{}{}", prefix, c.info.nick));
                }
            }
        }
        Some(nicks)
    }

    /// Get all channels a client is a member of.
    #[allow(dead_code)]
    pub async fn channels_for_client(&self, id: ClientId) -> Vec<String> {
        let cc = self.inner.membership_index.read().await;
        cc.get(&id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get local channel member handles excluding a given client.
    #[allow(dead_code)]
    pub async fn channel_members_except(
        &self,
        channel_name: &str,
        exclude: ClientId,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        let member_ids = arc.read().await.all_member_ids();

        let filtered: Vec<ClientId> = member_ids
            .into_iter()
            .filter(|&mid| mid != exclude)
            .collect();
        Some(self.local_handles_for_ids(&filtered).await)
    }

    /// Check channel send permissions and collect fan-out targets in a single
    /// lock acquisition — the hot path for PRIVMSG/NOTICE channel messages.
    ///
    /// Performs three checks sequentially under one channel read lock:
    ///   1. +n (no-external): sender must be a member.
    ///   2. +m (moderated): sender must be voiced or opped.
    ///   3. Fan-out: collect local member handles excluding the sender.
    ///
    /// Returns `ChannelSendResult` so the caller can issue the right error
    /// numerics without any further lock acquisitions.
    pub async fn check_channel_send(
        &self,
        channel_name: &str,
        sender_id: ClientId,
    ) -> ChannelSendResult {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return ChannelSendResult::NoSuchChannel,
        };

        // Collect everything we need from the channel under a single lock.
        let (no_external, moderated, is_member, is_op_or_voiced, member_ids) = {
            let ch = arc.read().await;
            let is_member = ch.is_member(sender_id);
            let is_op_or_voiced = ch.is_operator(sender_id) || ch.is_voiced(sender_id);
            // Collect member IDs directly from the members map — avoids the
            // all_member_ids() Vec alloc pattern (ON-7).
            let ids: Vec<ClientId> = ch.members.keys().copied().collect();
            (
                ch.modes.no_external,
                ch.modes.moderated,
                is_member,
                is_op_or_voiced,
                ids,
            )
        };

        // +n check.
        if no_external && !is_member {
            return ChannelSendResult::NoExternal;
        }

        // +m check.
        if moderated && !is_op_or_voiced {
            return ChannelSendResult::Moderated;
        }

        // Fan-out: collect local handles excluding sender, grouped by shard.
        let exclude_sender: Vec<ClientId> = member_ids
            .into_iter()
            .filter(|&mid| mid != sender_id)
            .collect();
        let members = self.local_handles_for_ids(&exclude_sender).await;

        ChannelSendResult::Ok(members)
    }

    // -- Channel operator / voice -------------------------------------------

    /// Check whether a client is an operator in a channel (by `ClientId`).
    pub async fn is_channel_operator(&self, channel_name: &str, id: ClientId) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        arc.read().await.is_operator(id)
    }

    /// Returns `true` if the given nick holds operator status in `channel_name`.
    pub async fn is_channel_operator_nick(&self, channel_name: &str, nick: &str) -> bool {
        // Resolve nick → ClientId first (O(1) via nick_index + single shard),
        // then acquire the channel lock — avoids holding a channel lock while
        // acquiring a user-shard lock (lock-ordering rule).
        let id = match self.find_user_by_nick(nick).await {
            Some(c) => c.id,
            None => return false,
        };
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        arc.read().await.is_operator(id)
    }

    /// Grant or revoke operator status for a nick in a channel.
    pub async fn set_channel_operator(
        &self,
        channel_name: &str,
        target_nick: &str,
        grant: bool,
    ) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        let Some(client) = self.find_user_by_nick(target_nick).await else {
            return false;
        };
        arc.write().await.set_operator(client.id, grant)
    }

    /// Remove a user from a channel by nick (kick). Returns remaining local member handles.
    pub async fn kick_from_channel(
        &self,
        channel_name: &str,
        target_nick: &str,
    ) -> Option<Vec<ClientHandle>> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();

        // Resolve nick → id.
        let client = self.find_user_by_nick(target_nick).await?;

        let (remaining_ids, is_empty) = {
            let mut ch = arc.write().await;
            if !ch.remove_member_by_id(client.id) {
                return None;
            }
            let remaining = ch.all_member_ids();
            let empty = ch.members.is_empty();
            (remaining, empty)
        };

        self.inner
            .membership_index
            .write()
            .await
            .entry(client.id)
            .or_default()
            .remove(&key);

        if is_empty {
            let mut map = self.inner.channels.write().await;
            if let Some(a) = map.get(&key)
                && a.read().await.members.is_empty() {
                    map.remove(&key);
                }
            return Some(vec![]);
        }

        Some(self.local_handles_for_ids(&remaining_ids).await)
    }

    // -- Channel modes ------------------------------------------------------

    /// Set or unset a channel mode flag. Returns `true` if the channel exists.
    pub async fn set_channel_mode(
        &self,
        channel_name: &str,
        mode_char: char,
        set: bool,
        param: Option<&str>,
    ) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        let mut ch = arc.write().await;
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
    }

    /// Get the mode string for a channel.
    pub async fn channel_mode_string(&self, channel_name: &str) -> Option<String> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        Some(arc.read().await.modes.to_mode_string())
    }

    /// Get the creation timestamp for a channel.
    pub async fn channel_created_at(&self, channel_name: &str) -> Option<u64> {
        let key = channel_name.to_ascii_lowercase();
        let arc = self.inner.channels.read().await.get(&key)?.clone();
        Some(arc.read().await.created_at)
    }

    /// Grant or revoke voice (+v) for a nick in a channel.
    pub async fn set_channel_voice(
        &self,
        channel_name: &str,
        target_nick: &str,
        grant: bool,
    ) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        let Some(client) = self.find_user_by_nick(target_nick).await else {
            return false;
        };
        arc.write().await.set_voice(client.id, grant)
    }

    /// Check whether a nick can speak in a channel (+m enforcement).
    #[allow(dead_code)]
    pub async fn can_speak_in_channel(&self, channel_name: &str, nick: &str) -> bool {
        // Resolve nick → ClientId before acquiring the channel lock.
        let id = match self.find_user_by_nick(nick).await {
            Some(c) => c.id,
            None => return false,
        };
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return true, // channel gone — no restriction
        };
        let ch = arc.read().await;
        if !ch.modes.moderated {
            return true;
        }
        ch.is_operator(id) || ch.is_voiced(id)
    }

    /// Check whether a nick is a member of a channel.
    #[allow(dead_code)]
    pub async fn is_channel_member(&self, channel_name: &str, nick: &str) -> bool {
        // Resolve nick first, then acquire channel lock (correct ordering).
        let id = match self.find_user_by_nick(nick).await {
            Some(c) => c.id,
            None => return false,
        };
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        arc.read().await.is_member(id)
    }

    /// Check whether a channel has +n (no external messages) mode set.
    #[allow(dead_code)]
    pub async fn channel_is_no_external(&self, channel_name: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        arc.read().await.modes.no_external
    }

    /// Check whether a channel is secret (+s).
    pub async fn channel_is_secret(&self, channel_name: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        arc.read().await.modes.secret
    }

    /// Get the count of active channels.
    #[allow(dead_code)]
    pub async fn channel_count(&self) -> usize {
        self.inner.channels.read().await.len()
    }

    /// List channels, filtering out secret (+s) channels for non-members.
    pub async fn list_channels_for(
        &self,
        client_id: ClientId,
    ) -> Vec<(String, usize, Option<String>)> {
        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();
        let mut result = Vec::with_capacity(arcs.len());
        for arc in arcs {
            let ch = arc.read().await;
            if !ch.modes.secret || ch.is_member(client_id) {
                let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
                result.push((ch.name.clone(), ch.member_count(), topic_text));
            }
        }
        result
    }

    /// Get channels for a client as seen by another client (for WHOIS).
    pub async fn channels_for_client_seen_by(
        &self,
        target_id: ClientId,
        querier_id: ClientId,
    ) -> Vec<String> {
        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();
        let mut result = Vec::new();
        for arc in arcs {
            let ch = arc.read().await;
            if ch.is_member(target_id) && (!ch.modes.secret || ch.is_member(querier_id)) {
                result.push(ch.name.clone());
            }
        }
        result
    }

    // -- Invite management --------------------------------------------------

    /// Add a nick to a channel's invite list. Returns `true` if the channel exists.
    pub async fn add_channel_invite(&self, channel_name: &str, nick: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        // Resolve nick → id.
        if let Some(client) = self.find_user_by_nick(nick).await {
            arc.write().await.add_invite(client.id);
        }
        true
    }

    /// Check whether a nick is on a channel's invite list.
    #[allow(dead_code)]
    pub async fn is_channel_invited(&self, channel_name: &str, nick: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return false,
        };
        if let Some(client) = self.find_user_by_nick(nick).await {
            arc.read().await.is_invited(client.id)
        } else {
            false
        }
    }

    /// Clear a nick from a channel's invite list (after successful join).
    pub async fn clear_channel_invite(&self, channel_name: &str, nick: &str) {
        let key = channel_name.to_ascii_lowercase();
        let arc = match self.inner.channels.read().await.get(&key).cloned() {
            Some(a) => a,
            None => return,
        };
        if let Some(client) = self.find_user_by_nick(nick).await {
            arc.write().await.clear_invite(client.id);
        }
    }

    /// Return `(local_id_handles, channel_ids)` for all local members in the
    /// given set of `ClientId`s.  Used by the relay snapshot builder.
    #[allow(dead_code)]
    pub async fn local_handles_for_ids_pub(&self, ids: &[ClientId]) -> Vec<ClientHandle> {
        self.local_handles_for_ids(ids).await
    }
}
