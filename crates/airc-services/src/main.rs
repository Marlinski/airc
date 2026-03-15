//! airc-services — NickServ and ChanServ as external IRC clients.
//!
//! This binary connects to an aircd server as separate IRC clients
//! (one per enabled service), authenticates via the OPER command to gain
//! service privileges (+S mode), and handles user commands via PRIVMSG.
//!
//! Each service is composed of togglable modules (configured in TOML).
//! Commands are dispatched through a [`module::ServiceDispatcher`] that
//! iterates modules until one handles the command.
//!
//! # Usage
//!
//! ```text
//! airc-services --config services.toml
//! ```

mod chanserv;
mod config;
mod module;
mod nickserv;

use std::path::Path;
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

/// AIRC Services — NickServ and ChanServ for aircd.
#[derive(Parser, Debug)]
#[command(name = "airc-services", about = "IRC services for aircd")]
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
struct ServiceBot {
    _name: String,
    client: IrcClient,
    events: mpsc::Receiver<IrcEvent>,
}

/// Connect a service bot to the IRC server and authenticate via OPER.
async fn connect_service(
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
        _name: nick.to_string(),
        client,
        events,
    })
}

/// Generic service event loop — dispatches incoming PRIVMSGs through a
/// [`ServiceDispatcher`] that routes to the appropriate module.
async fn run_service(
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

    let data_dir = Path::new(&cfg.data_dir);
    if !data_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            eprintln!("error: cannot create data directory {}: {e}", cfg.data_dir);
            std::process::exit(1);
        }
    }

    info!(
        server = %cfg.server_addr,
        oper_name = %cfg.oper_name,
        nickserv = cfg.nickserv_enabled,
        chanserv = cfg.chanserv_enabled,
        "starting airc-services"
    );

    let mut handles = Vec::new();

    // Start NickServ.
    if cfg.nickserv_enabled {
        match connect_service(&cfg.nickserv_nick, &cfg).await {
            Ok(bot) => {
                let state = Arc::new(nickserv::NickServState::new(bot.client.clone(), data_dir));
                let dispatcher = Arc::new(nickserv::create_dispatcher(
                    state,
                    &cfg.nickserv_modules,
                    &bot.client,
                ));
                let nick = cfg.nickserv_nick.clone();
                let client = bot.client;
                handles.push(tokio::spawn(run_service(
                    "NickServ".to_string(),
                    dispatcher,
                    client,
                    nick,
                    bot.events,
                )));
                info!("NickServ started as '{}'", cfg.nickserv_nick);
            }
            Err(e) => {
                error!(error = %e, "failed to connect NickServ");
                std::process::exit(1);
            }
        }
    }

    // Start ChanServ.
    if cfg.chanserv_enabled {
        match connect_service(&cfg.chanserv_nick, &cfg).await {
            Ok(bot) => {
                let state = Arc::new(chanserv::ChanServState::new(data_dir));
                let dispatcher = Arc::new(chanserv::create_dispatcher(
                    state,
                    &cfg.chanserv_modules,
                    &bot.client,
                ));
                let nick = cfg.chanserv_nick.clone();
                let client = bot.client;
                handles.push(tokio::spawn(run_service(
                    "ChanServ".to_string(),
                    dispatcher,
                    client,
                    nick,
                    bot.events,
                )));
                info!("ChanServ started as '{}'", cfg.chanserv_nick);
            }
            Err(e) => {
                error!(error = %e, "failed to connect ChanServ");
                std::process::exit(1);
            }
        }
    }

    if handles.is_empty() {
        eprintln!("error: no services enabled — nothing to do");
        std::process::exit(1);
    }

    // Wait for all service tasks to complete (they run indefinitely).
    for handle in handles {
        if let Err(e) = handle.await {
            error!(error = %e, "service task panicked");
        }
    }
}
