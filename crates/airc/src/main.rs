//! AIRC CLI — daemon + commands for AI agents to interact with IRC.
//!
//! The CLI works in two modes:
//! - **Daemon mode** (`airc connect`): starts a background process that
//!   maintains a persistent IRC connection and listens on a Unix socket.
//! - **Command mode** (all other subcommands): sends a request to the
//!   daemon via the Unix socket and prints the response.
//!
//! This two-process design means the IRC connection survives between agent
//! invocations. An agent just calls `airc fetch` to get new messages.

mod daemon;
mod ipc;

use clap::{Parser, Subcommand};

use airc_shared::ipc::ipc_request::Command;
use airc_shared::ipc::ipc_response::Payload;

#[derive(Parser)]
#[command(name = "airc", about = "AIRC — IRC for AI agents", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Connect to an IRC server (starts the daemon).
    Connect {
        /// Server address (host:port).
        #[arg(default_value = "localhost:6667")]
        server: String,

        /// Nickname to use.
        #[arg(short, long, default_value = "agent")]
        nick: String,

        /// Channels to auto-join (comma-separated).
        #[arg(short, long)]
        join: Option<String>,

        /// Run in foreground (don't daemonize).
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },

    /// Join a channel.
    Join {
        /// Channel name (e.g. #lobby).
        channel: String,
    },

    /// Leave a channel.
    Part {
        /// Channel name.
        channel: String,

        /// Optional reason.
        reason: Option<String>,
    },

    /// Send a message to a channel or user.
    Say {
        /// Target (channel or nick).
        target: String,

        /// Message text.
        message: String,
    },

    /// Fetch new (unread) messages.
    Fetch {
        /// Channel to fetch from (omit for all channels).
        channel: Option<String>,

        /// Fetch last N messages instead of only unread.
        #[arg(long)]
        last: Option<u32>,

        /// Output as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Show connection status and channel info.
    Status {
        /// Output as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Disconnect from the server (stops the daemon).
    Disconnect,

    /// Control client-side CSV logging.
    Log {
        #[command(subcommand)]
        action: LogAction,
    },

    /// Show recent log events from the daemon's in-memory buffer.
    Logs {
        /// Number of recent events to show (default: 50).
        #[arg(short = 'n', long, default_value_t = 50)]
        last: u32,

        /// Filter by channel name.
        #[arg(short, long)]
        channel: Option<String>,

        /// Output as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum LogAction {
    /// Start logging messages to CSV files.
    Start {
        /// Directory to write log files (defaults to current directory).
        #[arg(short, long)]
        dir: Option<String>,
    },
    /// Stop logging.
    Stop,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Connect {
            server,
            nick,
            join,
            foreground,
        } => {
            let auto_join: Vec<String> = join
                .map(|j| j.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();

            if let Err(e) = daemon::start(server, nick, auto_join, foreground).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }

        Commands::Join { channel } => {
            send_command(Command::Join(ipc::JoinRequest { channel })).await;
        }

        Commands::Part { channel, reason } => {
            send_command(Command::Part(ipc::PartRequest { channel, reason })).await;
        }

        Commands::Say { target, message } => {
            send_command(Command::Say(ipc::SayRequest { target, message })).await;
        }

        Commands::Fetch {
            channel,
            last,
            json,
        } => {
            let resp =
                send_command(Command::Fetch(ipc::FetchRequest { channel, last })).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else {
                print_response(&resp);
            }
        }

        Commands::Status { json } => {
            let resp =
                send_command(Command::Status(ipc::StatusRequest {})).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else {
                print_response(&resp);
            }
        }

        Commands::Disconnect => {
            send_command(Command::Disconnect(ipc::DisconnectRequest {})).await;
        }

        Commands::Log { action } => match action {
            LogAction::Start { dir } => {
                send_command(Command::LogStart(ipc::LogStartRequest { dir })).await;
            }
            LogAction::Stop => {
                send_command(Command::LogStop(ipc::LogStopRequest {})).await;
            }
        },

        Commands::Logs { last, channel, json } => {
            let resp = send_command(Command::Logs(ipc::LogsRequest {
                last: Some(last),
                channel,
            })).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else {
                print_response(&resp);
            }
        },
    }
}

/// Build an IpcRequest from a command, send it, and return the response.
async fn send_command(command: Command) -> ipc::IpcResponse {
    let req = ipc::IpcRequest {
        command: Some(command),
    };
    match ipc::send_request(&req).await {
        Ok(resp) => {
            if let Some(ref err) = resp.error {
                eprintln!("error: {err}");
            }
            resp
        }
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("hint:  start the daemon first with `airc connect`");
            std::process::exit(1);
        }
    }
}

/// Pretty-print a response for humans.
fn print_response(resp: &ipc::IpcResponse) {
    if let Some(ref err) = resp.error {
        eprintln!("error: {err}");
        return;
    }

    match &resp.payload {
        Some(Payload::Text(t)) => {
            println!("{}", t.text);
        }

        Some(Payload::Messages(fetch)) => {
            if fetch.messages.is_empty() {
                println!("(no new messages)");
            } else {
                for msg in &fetch.messages {
                    let ts = chrono_format(msg.timestamp);
                    let kind = airc_client::MessageKind::try_from(msg.kind)
                        .unwrap_or(airc_client::MessageKind::Normal);
                    match kind {
                        airc_client::MessageKind::Action => {
                            println!("[{ts}] {}: * {} {}", msg.target, msg.from, msg.text);
                        }
                        airc_client::MessageKind::Normal => {
                            println!("[{ts}] {}: <{}> {}", msg.target, msg.from, msg.text);
                        }
                    }
                }
            }
        }

        Some(Payload::Channels(status)) => {
            println!("connected as {}", status.nick);
            if status.channels.is_empty() {
                println!("(not in any channels)");
            } else {
                for ch in &status.channels {
                    let topic = ch.topic.as_deref().unwrap_or("(no topic)");
                    println!(
                        "  {} — {} members, {} unread — {}",
                        ch.name, ch.members, ch.unread, topic
                    );
                }
            }
        }

        Some(Payload::Logs(logs)) => {
            if logs.events.is_empty() {
                println!("(no log events)");
            } else {
                for ev in &logs.events {
                    let etype = airc_shared::log::event_type_to_str(ev.event_type);
                    println!(
                        "[{}] {} {} <{}> {}",
                        ev.timestamp, ev.channel, etype, ev.nick, ev.content
                    );
                }
            }
        }

        None => {
            // No payload — just the ok/error fields handled above.
        }
    }
}

/// Format a unix timestamp into a compact time string.
fn chrono_format(ts: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(ts);
    let elapsed = dt
        .elapsed()
        .unwrap_or_default()
        .as_secs();
    if elapsed < 60 {
        format!("{elapsed}s ago")
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}
