//! `aircd-redis-logger` — Redis Pub/Sub logging sidecar for aircd clusters.
//!
//! Subscribes to the `airc:relay` Redis channel and writes a complete,
//! deduplicated CSV event log for the entire cluster. Because this process
//! is the *only* subscriber writing logs, there is no duplication regardless
//! of how many `aircd` nodes are running.
//!
//! # Configuration (environment variables)
//!
//! | Variable              | Default                       | Description                       |
//! |-----------------------|-------------------------------|-----------------------------------|
//! | `AIRCD_REDIS_URL`     | `redis://127.0.0.1:6379`      | Redis connection URL              |
//! | `AIRCD_LOG_DIR`       | `./logs`                      | Directory to write CSV log files  |
//! | `AIRCD_LOGGER_NODE_ID`| `logger`                      | node_id stamped on log rows       |
//! | `RUST_LOG`            | `info`                        | Tracing filter                    |
//!
//! # CSV log format
//!
//! ```text
//! seq,node_id,timestamp,event_type,channel,nick,content
//! ```
//!
//! The `node_id` column identifies the *originating* aircd node, not this
//! logger process.

use std::path::PathBuf;

use futures_util::StreamExt;
use prost::Message as ProstMessage;
use tracing::{error, info, warn};

use airc_shared::log::FileLogger;
use airc_shared::relay::{RELAY_CHANNEL, RelayEnvelope, RelayEvent};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // -- Tracing -------------------------------------------------------------
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // -- Config from env vars ------------------------------------------------
    let redis_url =
        std::env::var("AIRCD_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let log_dir = std::env::var("AIRCD_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./logs"));
    let logger_node_id =
        std::env::var("AIRCD_LOGGER_NODE_ID").unwrap_or_else(|_| "logger".to_string());

    info!(
        redis_url = %redis_url,
        log_dir = %log_dir.display(),
        node_id = %logger_node_id,
        "aircd-redis-logger starting"
    );

    // -- Redis connection ----------------------------------------------------
    let client = match redis::Client::open(redis_url.as_str()) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to open redis client: {e}");
            std::process::exit(1);
        }
    };

    let pubsub_conn = match client.get_async_pubsub().await {
        Ok(c) => c,
        Err(e) => {
            error!("failed to connect to redis: {e}");
            std::process::exit(1);
        }
    };

    let mut pubsub_conn = pubsub_conn;
    if let Err(e) = pubsub_conn.subscribe(RELAY_CHANNEL).await {
        error!("failed to subscribe to {RELAY_CHANNEL}: {e}");
        std::process::exit(1);
    }

    info!(
        "subscribed to {RELAY_CHANNEL} — writing logs to {}",
        log_dir.display()
    );

    // -- Logger --------------------------------------------------------------
    // One FileLogger per originating node_id — each stamps its own node_id on
    // every CSV row, giving per-node log files with correct attribution.
    let mut loggers: std::collections::HashMap<String, FileLogger> =
        std::collections::HashMap::new();

    // -- Event loop ----------------------------------------------------------
    let mut stream = pubsub_conn.on_message();
    loop {
        let msg = match stream.next().await {
            Some(m) => m,
            None => {
                warn!("redis pubsub stream ended — exiting");
                break;
            }
        };

        let payload: Vec<u8> = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                warn!("redis msg payload error: {e}");
                continue;
            }
        };

        let envelope = match RelayEnvelope::decode(payload.as_slice()) {
            Ok(e) => e,
            Err(e) => {
                warn!("protobuf decode error: {e}");
                continue;
            }
        };

        let origin = envelope.node_id.clone();

        let event = match envelope.event {
            Some(ev) => ev,
            None => {
                warn!("relay envelope from {origin} has no event");
                continue;
            }
        };

        // Ensure a FileLogger exists for this origin node.
        let logger = loggers
            .entry(origin.clone())
            .or_insert_with(|| FileLogger::new(Some(log_dir.clone()), origin.clone()));

        match event {
            // -----------------------------------------------------------------
            // Client lifecycle
            // -----------------------------------------------------------------
            RelayEvent::ClientIntro(e) => {
                let line = format!(
                    "INTRO {}!{}@{} (client_id={}, node={})",
                    e.nick, e.user, e.host, e.client_id, origin
                );
                info!("{line}");
                logger.log_join("*", &e.nick);
            }
            RelayEvent::ClientDown(e) => {
                let reason = if e.reason.is_empty() {
                    String::new()
                } else {
                    format!(" reason={}", e.reason)
                };
                let line = format!("DOWN client_id={}{}", e.client_id, reason);
                info!("{line}");
                logger.log_quit("*", &e.client_id, &e.reason);
            }

            // -----------------------------------------------------------------
            // Nick change
            // -----------------------------------------------------------------
            RelayEvent::NickChange(e) => {
                let line = format!("NICK client_id={} -> {}", e.client_id, e.new_nick);
                info!("{line}");
                logger.log_nick_change("*", &e.client_id, &e.new_nick);
            }

            // -----------------------------------------------------------------
            // Channel events
            // -----------------------------------------------------------------
            RelayEvent::Join(e) => {
                let line = format!("JOIN client_id={} channel={}", e.client_id, e.channel);
                info!("{line}");
                logger.log_join(&e.channel, &e.client_id);
            }
            RelayEvent::Part(e) => {
                let line = format!(
                    "PART client_id={} channel={} reason={}",
                    e.client_id, e.channel, e.reason
                );
                info!("{line}");
                logger.log_part(&e.channel, &e.client_id, &e.reason);
            }
            RelayEvent::Quit(e) => {
                let line = format!("QUIT client_id={} reason={}", e.client_id, e.reason);
                info!("{line}");
                logger.log_quit("*", &e.client_id, &e.reason);
            }

            // -----------------------------------------------------------------
            // Messaging
            // -----------------------------------------------------------------
            RelayEvent::Privmsg(e) => {
                use airc_shared::relay_proto::privmsg::Target;
                let target = match &e.target {
                    Some(Target::TargetChannel(ch)) => ch.clone(),
                    Some(Target::TargetClientId(id)) => format!("@{id}"),
                    None => String::from("<unknown>"),
                };
                let line = format!(
                    "PRIVMSG client_id={} -> {}: {}",
                    e.client_id, target, e.text
                );
                info!("{line}");
                if target.starts_with('#') || target.starts_with('&') {
                    logger.log_message(&target, &e.client_id, &e.text);
                }
            }
            RelayEvent::Notice(e) => {
                use airc_shared::relay_proto::notice::Target;
                let target = match &e.target {
                    Some(Target::TargetChannel(ch)) => ch.clone(),
                    Some(Target::TargetClientId(id)) => format!("@{id}"),
                    None => String::from("<unknown>"),
                };
                let line = format!("NOTICE client_id={} -> {}: {}", e.client_id, target, e.text);
                info!("{line}");
                if target.starts_with('#') || target.starts_with('&') {
                    logger.log_notice(&target, &e.client_id, &e.text);
                }
            }

            // -----------------------------------------------------------------
            // Channel state changes
            // -----------------------------------------------------------------
            RelayEvent::Topic(e) => {
                let line = format!(
                    "TOPIC client_id={} channel={} text={}",
                    e.client_id, e.channel, e.text
                );
                info!("{line}");
                logger.log_topic(&e.channel, &e.client_id, &e.text);
            }
            RelayEvent::Mode(e) => {
                let line = format!(
                    "MODE client_id={} target={} mode={}",
                    e.client_id, e.target, e.mode_string
                );
                info!("{line}");
            }
            RelayEvent::Kick(e) => {
                let content = format!("by {} reason={}", e.client_id, e.reason);
                let line = format!(
                    "KICK client_id={} channel={} target={} reason={}",
                    e.client_id, e.channel, e.target_client_id, e.reason
                );
                info!("{line}");
                logger.log_kick(&e.channel, &e.target_client_id, &content);
            }

            // -----------------------------------------------------------------
            // Node lifecycle
            // -----------------------------------------------------------------
            RelayEvent::NodeUp(e) => {
                info!("node up: {}", e.node_id);
            }
            RelayEvent::NodeDown(e) => {
                info!("node down: {}", e.node_id);
            }

            // -----------------------------------------------------------------
            // CRDT / anti-entropy / state snapshot — no loggable IRC content
            // -----------------------------------------------------------------
            RelayEvent::CrdtDelta(_)
            | RelayEvent::AntiEntropyRequest(_)
            | RelayEvent::AntiEntropyResponse(_)
            | RelayEvent::StateSnapshot(_) => {}
        }
    }
}
