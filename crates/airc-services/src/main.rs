//! airc-services — external service bot framework for aircd.
//!
//! NickServ and ChanServ are now embedded directly inside aircd (Phase A).
//! This binary is kept as the foundation for **third-party / custom service
//! bots** that connect to aircd as IRC clients.
//!
//! # Usage
//!
//! ```text
//! airc-services --config services.toml
//! ```

mod config;
mod module;

use std::sync::Arc;

use clap::Parser;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use airc_client::{ClientConfig, IrcClient, IrcEvent};

use crate::config::{CliOverrides, ServicesConfig};
use crate::module::ServiceDispatcher;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// AIRC Services — external service bot framework for aircd.
#[derive(Parser, Debug)]
#[command(
    name = "airc-services",
    about = "External IRC service bot framework for aircd"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long)]
    config: Option<String>,

    /// IRC server address (host:port).
    #[arg(long)]
    server: Option<String>,

    /// Operator name for OPER authentication.
    #[arg(long)]
    oper_name: Option<String>,

    /// Operator password for OPER authentication.
    #[arg(long)]
    oper_password: Option<String>,

    /// Directory for persistence files.
    #[arg(long)]
    data_dir: Option<String>,

    /// TLS mode: required, preferred, disabled.
    #[arg(long)]
    tls: Option<String>,
}

// ---------------------------------------------------------------------------
// Service bot runner
// ---------------------------------------------------------------------------

/// A running service bot with its client and event receiver.
pub struct ServiceBot {
    pub name: String,
    pub client: IrcClient,
    pub events: mpsc::Receiver<IrcEvent>,
}

/// Connect a service bot to the IRC server and authenticate via OPER.
pub async fn connect_service(
    nick: &str,
    cfg: &ServicesConfig,
) -> Result<ServiceBot, Box<dyn std::error::Error>> {
    let client_config = ClientConfig::new(&cfg.server_addr, nick)
        .with_username(nick)
        .with_realname(&format!("AIRC {nick} Service"))
        .with_tls(cfg.tls);

    let client_config = if let Some(ref pass) = cfg.server_password {
        client_config.with_password(pass)
    } else {
        client_config
    };

    info!(nick = %nick, server = %cfg.server_addr, "connecting service bot");
    let (client, _motd, events) = IrcClient::connect(client_config).await?;

    // Authenticate as operator to gain +S (service) mode.
    info!(nick = %nick, oper_name = %cfg.oper_name, "sending OPER command");
    client.send_oper(&cfg.oper_name, &cfg.oper_password).await?;

    Ok(ServiceBot {
        name: nick.to_string(),
        client,
        events,
    })
}

/// Generic service event loop — dispatches incoming PRIVMSGs through a
/// [`ServiceDispatcher`] that routes to the appropriate module.
pub async fn run_service(
    service_name: String,
    dispatcher: Arc<ServiceDispatcher>,
    client: IrcClient,
    expected_nick: String,
    mut events: mpsc::Receiver<IrcEvent>,
) {
    info!(service = %service_name, "event loop started");
    loop {
        match events.recv().await {
            Some(IrcEvent::Message(msg)) => {
                // Only handle messages directed at us (PRIVMSGs to our nick).
                if !msg.target.eq_ignore_ascii_case(&expected_nick) {
                    continue;
                }
                dispatcher.dispatch(&msg.from, &msg.text, &client).await;
            }
            Some(IrcEvent::Disconnected { reason }) => {
                warn!(service = %service_name, reason = %reason, "disconnected (will auto-reconnect)");
            }
            Some(IrcEvent::Reconnected) => {
                info!(service = %service_name, "reconnected");
                // TODO: re-send OPER after reconnect
            }
            Some(_) => {
                // Ignore other events.
            }
            None => {
                error!(service = %service_name, "event channel closed, shutting down");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Load configuration.
    let cfg = ServicesConfig::load(
        cli.config.as_deref(),
        CliOverrides {
            server: cli.server,
            oper_name: cli.oper_name,
            oper_password: cli.oper_password,
            data_dir: cli.data_dir,
            tls: cli.tls,
        },
    );

    if cfg.oper_password.is_empty() {
        eprintln!(
            "error: oper_password is required (set in config file, AIRC_SERVICES_OPER_PASSWORD env var, or --oper-password flag)"
        );
        std::process::exit(1);
    }

    // NickServ and ChanServ are now embedded in aircd (Phase A).
    // Add custom service bots here by calling connect_service() and run_service().
    info!(
        server = %cfg.server_addr,
        "airc-services started (no external bots configured — add custom bots here)"
    );

    // Nothing to run — exit cleanly.
    // In a real deployment, custom service bots would be registered here.
    info!("no service bots registered; exiting");
}
