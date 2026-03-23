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

use airc_client::{DEFAULT_NICK, DEFAULT_SERVER, TlsMode};
use airc_shared::ipc::ipc_request::Command;
use airc_shared::ipc::ipc_response::Payload;

#[derive(Parser)]
#[command(name = "airc", about = "AIRC — IRC for AI agents", version)]
struct Cli {
    /// Session ID to target (when multiple sessions exist in the same directory).
    #[arg(short = 's', long = "session", global = true)]
    session: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Connect to an IRC server (starts the daemon).
    Connect {
        /// Server address (host:port).
        #[arg(default_value = DEFAULT_SERVER)]
        server: String,

        /// Nickname to use.
        #[arg(short, long, default_value = DEFAULT_NICK)]
        nick: String,

        /// Channels to auto-join (comma-separated).
        #[arg(short, long)]
        join: Option<String>,

        /// Require TLS (fail if TLS handshake fails).
        #[arg(long, conflicts_with = "no_tls")]
        tls: bool,

        /// Disable TLS (plain TCP only).
        #[arg(long, conflicts_with = "tls")]
        no_tls: bool,

        /// Password for NickServ/SASL authentication.
        #[arg(long, short = 'p')]
        password: Option<String>,

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

    /// Silence a user (stop receiving their messages).
    Silence {
        /// Nick of the user to silence.
        nick: Option<String>,

        /// Reason for silencing.
        #[arg(short, long)]
        reason: Option<String>,

        /// List currently silenced nicks.
        #[arg(long, default_value_t = false)]
        list: bool,
    },

    /// Unsilence a user (resume receiving their messages).
    Unsilence {
        /// Nick of the user to unsilence.
        nick: String,
    },

    /// Add a user as a friend (reputation boost).
    Friend {
        /// Nick of the user to friend.
        nick: Option<String>,

        /// List current friends.
        #[arg(long, default_value_t = false)]
        list: bool,
    },

    /// Remove a user from your friend list.
    Unfriend {
        /// Nick of the user to unfriend.
        nick: String,
    },

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

    /// Register a new nickname with NickServ.
    ///
    /// Connects to the server, sends `PRIVMSG NickServ :REGISTER <password>`,
    /// waits for a NickServ NOTICE confirming success or failure, then exits.
    /// No daemon is started.
    Register {
        /// Server address (host:port).
        #[arg(default_value = DEFAULT_SERVER)]
        server: String,

        /// Nickname to register.
        #[arg(short, long, default_value = DEFAULT_NICK)]
        nick: String,

        /// Password for the new NickServ account.
        password: String,

        /// Require TLS (fail if TLS handshake fails).
        #[arg(long, conflicts_with = "no_tls")]
        tls: bool,

        /// Disable TLS (plain TCP only).
        #[arg(long, conflicts_with = "tls")]
        no_tls: bool,
    },

    /// Start the MCP server (stdio transport).
    ///
    /// Runs an MCP server over stdin/stdout for use with AI agent hosts
    /// like Claude Desktop, OpenCode, or Cursor. All other airc commands
    /// are exposed as MCP tools.
    Mcp,
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
            tls,
            no_tls,
            password,
            foreground,
        } => {
            let auto_join: Vec<String> = join
                .map(|j| j.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();

            let tls_mode = if tls {
                TlsMode::Required
            } else if no_tls {
                TlsMode::Disabled
            } else {
                TlsMode::Preferred
            };

            // If a session was explicitly provided via --session, use it.
            // Otherwise, check if an active session already exists in cwd
            // — if so, error out. If not, generate a new session ID.
            let cwd = std::env::current_dir().unwrap_or_else(|e| {
                eprintln!("error: cannot determine working directory: {e}");
                std::process::exit(1);
            });

            let session_id = if let Some(ref id) = cli.session {
                id.clone()
            } else {
                // Check for any existing live session in cwd.
                if let Ok(info) = ipc::discover_socket(&cwd, None) {
                    if tokio::net::UnixStream::connect(&info.path).await.is_ok() {
                        eprintln!(
                            "error: already connected (session {}). \
                             Use `airc disconnect` first, or `--session <id>` to start a new session.",
                            info.session_id
                        );
                        std::process::exit(1);
                    }
                    // Stale socket — clean it up.
                    let _ = std::fs::remove_file(&info.path);
                }
                ipc::generate_session_id()
            };

            if let Err(e) = daemon::start(
                session_id.clone(),
                server,
                nick,
                auto_join,
                tls_mode,
                password,
                foreground,
            )
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }

            // Print the session ID so callers can capture it.
            eprintln!("session: {session_id}");
        }

        Commands::Join { channel } => {
            let sock = discover_or_exit(&cli.session);
            send_command(&sock, Command::Join(ipc::JoinRequest { channel })).await;
        }

        Commands::Part { channel, reason } => {
            let sock = discover_or_exit(&cli.session);
            send_command(&sock, Command::Part(ipc::PartRequest { channel, reason })).await;
        }

        Commands::Say { target, message } => {
            let sock = discover_or_exit(&cli.session);
            send_command(&sock, Command::Say(ipc::SayRequest { target, message })).await;
        }

        Commands::Fetch {
            channel,
            last,
            json,
        } => {
            let sock = discover_or_exit(&cli.session);
            let resp =
                send_command(&sock, Command::Fetch(ipc::FetchRequest { channel, last })).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else {
                print_response(&resp);
            }
        }

        Commands::Status { json } => {
            let sock = discover_or_exit(&cli.session);
            let resp = send_command(&sock, Command::Status(ipc::StatusRequest {})).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else {
                print_response(&resp);
            }
        }

        Commands::Disconnect => {
            let sock = discover_or_exit(&cli.session);
            send_command(&sock, Command::Disconnect(ipc::DisconnectRequest {})).await;
            // The daemon removes the socket file on shutdown, but clean up
            // from our side too in case it didn't get the chance.
            let _ = std::fs::remove_file(&sock);
        }

        Commands::Silence { nick, reason, list } => {
            let sock = discover_or_exit(&cli.session);
            if list {
                send_command(
                    &sock,
                    Command::Silence(ipc::SilenceRequest {
                        nick: String::new(),
                        remove: false,
                        list: true,
                        reason: None,
                    }),
                )
                .await;
            } else if let Some(nick) = nick {
                send_command(
                    &sock,
                    Command::Silence(ipc::SilenceRequest {
                        nick,
                        remove: false,
                        list: false,
                        reason,
                    }),
                )
                .await;
            } else {
                eprintln!("error: provide a nick to silence, or use --list");
                std::process::exit(1);
            }
        }

        Commands::Unsilence { nick } => {
            let sock = discover_or_exit(&cli.session);
            send_command(
                &sock,
                Command::Silence(ipc::SilenceRequest {
                    nick,
                    remove: true,
                    list: false,
                    reason: None,
                }),
            )
            .await;
        }

        Commands::Friend { nick, list } => {
            let sock = discover_or_exit(&cli.session);
            if list {
                send_command(
                    &sock,
                    Command::Friend(ipc::FriendRequest {
                        nick: String::new(),
                        remove: false,
                        list: true,
                    }),
                )
                .await;
            } else if let Some(nick) = nick {
                send_command(
                    &sock,
                    Command::Friend(ipc::FriendRequest {
                        nick,
                        remove: false,
                        list: false,
                    }),
                )
                .await;
            } else {
                eprintln!("error: provide a nick to friend, or use --list");
                std::process::exit(1);
            }
        }

        Commands::Unfriend { nick } => {
            let sock = discover_or_exit(&cli.session);
            send_command(
                &sock,
                Command::Friend(ipc::FriendRequest {
                    nick,
                    remove: true,
                    list: false,
                }),
            )
            .await;
        }

        Commands::Log { action } => {
            let sock = discover_or_exit(&cli.session);
            match action {
                LogAction::Start { dir } => {
                    send_command(&sock, Command::LogStart(ipc::LogStartRequest { dir })).await;
                }
                LogAction::Stop => {
                    send_command(&sock, Command::LogStop(ipc::LogStopRequest {})).await;
                }
            }
        }

        Commands::Logs {
            last,
            channel,
            json,
        } => {
            let sock = discover_or_exit(&cli.session);
            let resp = send_command(
                &sock,
                Command::Logs(ipc::LogsRequest {
                    last: Some(last),
                    channel,
                }),
            )
            .await;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else {
                print_response(&resp);
            }
        }

        Commands::Register {
            server,
            nick,
            password,
            tls,
            no_tls,
        } => {
            let tls_mode = if tls {
                TlsMode::Required
            } else if no_tls {
                TlsMode::Disabled
            } else {
                TlsMode::Preferred
            };
            if let Err(e) = cmd_register(server, nick, password, tls_mode).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }

        Commands::Mcp => {
            // The MCP server uses stdio for JSON-RPC, so tracing must go
            // to stderr. Set it up here before handing off.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();

            if let Err(e) = airc_mcp::run().await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Discover the session socket in cwd, or exit with a helpful error.
fn discover_or_exit(session: &Option<String>) -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("error: cannot determine working directory: {e}");
        std::process::exit(1);
    });
    match ipc::discover_socket(&cwd, session.as_deref()) {
        Ok(info) => info.path,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Build an IpcRequest from a command, send it, and return the response.
async fn send_command(sock_path: &std::path::Path, command: Command) -> ipc::IpcResponse {
    let req = ipc::IpcRequest {
        command: Some(command),
    };
    match ipc::send_request(sock_path, &req).await {
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
    let elapsed = dt.elapsed().unwrap_or_default().as_secs();
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

/// One-shot NickServ REGISTER command.
///
/// Connects to `server` as `nick` (no password), waits for RPL_WELCOME,
/// sends `PRIVMSG NickServ :REGISTER <password>`, then waits up to 10 s
/// for a NOTICE from NickServ.  Prints the NOTICE text and returns.
async fn cmd_register(
    server: String,
    nick: String,
    password: String,
    tls_mode: TlsMode,
) -> Result<(), Box<dyn std::error::Error>> {
    use airc_client::{ClientConfig, IrcClient, IrcEvent};
    use tokio::time::{Duration, timeout};

    let config = ClientConfig::new(&server, &nick).with_tls(tls_mode);

    let (client, _motd, mut event_rx) = IrcClient::connect(config).await?;

    // Send the REGISTER command.
    client
        .say("NickServ", &format!("REGISTER {password}"))
        .await?;

    // Wait up to 10 s for a NOTICE from NickServ.
    let notice_timeout = Duration::from_secs(10);
    let result = timeout(notice_timeout, async {
        loop {
            match event_rx.recv().await {
                Some(IrcEvent::Notice { from, text, .. }) => {
                    let sender = from.as_deref().unwrap_or("");
                    // NickServ may appear as "NickServ" or "NickServ!NickServ@services"
                    if sender.eq_ignore_ascii_case("NickServ") || sender.starts_with("NickServ!") {
                        return Some(text);
                    }
                }
                Some(IrcEvent::Disconnected { reason }) => {
                    return Some(format!("disconnected: {reason}"));
                }
                None => return None,
                _ => {}
            }
        }
    })
    .await;

    // Quit cleanly regardless of outcome.
    let _ = client.quit(None).await;

    match result {
        Ok(Some(text)) => {
            println!("{text}");
            Ok(())
        }
        Ok(None) => Err("connection closed before NickServ responded".into()),
        Err(_) => Err("timed out waiting for NickServ response".into()),
    }
}
