//! Stats cache and HTTP / IPC / Prometheus query methods.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use airc_shared::aircd_ipc;

use crate::channel::Channel;
use crate::web;

use super::{PrometheusStats, SharedState};

impl SharedState {
    // -- Stats cache --------------------------------------------------------

    /// Populate / refresh the stats cache if stale.
    ///
    /// Acquires `stats_cache` write lock only when the TTL has expired; all
    /// callers that arrive within the TTL window take only a read lock.
    ///
    /// Lock ordering: user shards read → channels map read → per-channel read
    ///                → stats_cache write.
    pub(super) async fn ensure_stats_cache(&self) {
        // Fast path: cache is still fresh — no work needed.
        if self.inner.stats_cache.read().await.is_fresh() {
            return;
        }

        // Slow path: recompute under write lock.  A second concurrent caller
        // may also reach here; that is acceptable — the worst case is two
        // redundant refreshes within the same TTL window, not a correctness
        // issue.
        let mut users_online = 0u64;
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            users_online += shard.values().filter(|c| c.is_local()).count() as u64;
        }

        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();

        let channels_active = arcs.len() as u64;
        let mut channel_info: Vec<web::ChannelInfo> = Vec::with_capacity(arcs.len());
        let mut channel_counts: Vec<(String, u64)> = Vec::with_capacity(arcs.len());

        for arc in arcs {
            let ch = arc.read().await;
            let member_count = ch.member_count() as u64;
            let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
            let modes = ch.modes.to_mode_string();
            channel_counts.push((ch.name.clone(), member_count));
            channel_info.push(web::ChannelInfo {
                name: ch.name.clone(),
                topic: topic_text,
                member_count,
                modes,
                description: None,
                min_reputation: None,
            });
        }
        channel_info.sort_by(|a, b| a.name.cmp(&b.name));

        let mut cache = self.inner.stats_cache.write().await;
        cache.users_online = users_online;
        cache.channels_active = channels_active;
        cache.channel_info = channel_info;
        cache.channel_counts = channel_counts;
        cache.computed_at = Some(Instant::now());
    }

    // -- HTTP API queries ---------------------------------------------------

    /// Server stats for `GET /api/stats`.
    pub async fn api_stats(&self) -> web::StatsResponse {
        self.ensure_stats_cache().await;
        let cache = self.inner.stats_cache.read().await;
        web::StatsResponse {
            server_name: self.inner.config.server_name.clone(),
            users_online: cache.users_online,
            channels_active: cache.channels_active,
            uptime_seconds: self.inner.started_at.elapsed().as_secs(),
        }
    }

    /// Channel listing for `GET /api/channels`.
    pub async fn api_channels(&self) -> Vec<web::ChannelInfo> {
        self.ensure_stats_cache().await;
        self.inner.stats_cache.read().await.channel_info.clone()
    }

    // -- IPC / Prometheus queries -------------------------------------------

    /// Lightweight stats snapshot for Prometheus `/metrics` scrapes.
    pub async fn prometheus_stats(&self) -> PrometheusStats {
        self.ensure_stats_cache().await;
        let cache = self.inner.stats_cache.read().await;
        PrometheusStats {
            users_online: cache.users_online,
            channels_active: cache.channels_active,
            uptime_seconds: self.inner.started_at.elapsed().as_secs(),
            channel_counts: cache.channel_counts.clone(),
        }
    }

    /// Full server stats for IPC (`aircd status`) and Prometheus metrics.
    pub async fn stats(&self) -> aircd_ipc::StatsResponse {
        let mut users = 0usize;
        for shard_lock in &self.inner.users {
            let shard = shard_lock.read().await;
            users += shard.values().filter(|c| c.is_local()).count();
        }
        let arcs: Vec<Arc<RwLock<Channel>>> =
            self.inner.channels.read().await.values().cloned().collect();

        let channels_active = arcs.len() as u64;
        let mut channel_list = Vec::with_capacity(arcs.len());
        for arc in arcs {
            let ch = arc.read().await;
            let topic_text = ch.topic.as_ref().map(|(t, _, _)| t.clone());
            let modes = ch.modes.to_mode_string();
            channel_list.push(aircd_ipc::ChannelInfo {
                name: ch.name.clone(),
                topic: topic_text,
                member_count: ch.member_count() as u64,
                modes,
                description: None,
                min_reputation: None,
            });
        }

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
