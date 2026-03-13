//! The AIRC daemon — maintains a persistent IRC connection and serves
//! CLI requests over a Unix socket.
//!
//! Client-side CSV logging is toggled at runtime via `airc log start/stop`.
//! When active, the daemon logs every IRC event to CSV files (one per
//! channel/DM) using the shared [`FileLogger`] from `airc-shared`.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;
use tracing::{error, info};

use airc_client::{IrcClient, IrcEvent};
use airc_shared::ipc::ipc_request::Command;
use airc_shared::log::{log_event_now, EventType, FileLogger, LogEvent};

use crate::ipc::{self, IpcRequest, IpcResponse};

/// Shared handle to the optional client-side logger.
///
/// `None` means logging is off. Wrapped in `Arc<Mutex<..>>` so the event
/// loop and IPC handlers can share it.
type SharedLogger = Arc<Mutex<Option<FileLogger>>>;

/// In-memory ring buffer of recent log events for `airc logs`.
///
/// Always active (unlike the CSV FileLogger). Stores the last N events so
/// CLI and MCP clients can retrieve recent history.
const LOG_RING_CAPACITY: usize = 500;

type SharedLogRing = Arc<Mutex<VecDeque<LogEvent>>>;

/// Flag set by the `disconnect` command to signal intentional shutdown.
/// Prevents auto-reconnect from kicking in after a deliberate QUIT.
type ShutdownFlag = Arc<AtomicBool>;

/// Create a new shared log ring buffer.
fn new_log_ring() -> SharedLogRing {
    Arc::new(Mutex::new(VecDeque::with_capacity(LOG_RING_CAPACITY)))
}

/// Push an event into the ring buffer, evicting the oldest if full.
fn push_log_ring(ring: &SharedLogRing, event: LogEvent) {
    if let Ok(mut buf) = ring.lock() {
        if buf.len() >= LOG_RING_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(event);
    }
}

/// Read events from the ring buffer, optionally filtered by channel and limited.
fn read_log_ring(ring: &SharedLogRing, last: Option<usize>, channel: Option<&str>) -> Vec<LogEvent> {
    let Ok(buf) = ring.lock() else {
        return Vec::new();
    };
    let filtered: Vec<_> = if let Some(ch) = channel {
        buf.iter()
            .filter(|e| e.channel.eq_ignore_ascii_case(ch))
            .cloned()
            .collect()
    } else {
        buf.iter().cloned().collect()
    };
    let n = last.unwrap_or(50);
    let start = filtered.len().saturating_sub(n);
    filtered[start..].to_vec()
}

/// Start the daemon: connect to IRC, then listen for CLI commands.
pub async fn start(
    server: String,
    nick: String,
    auto_join: Vec<String>,
    foreground: bool,
) -> Result<(), String> {
    // Check if daemon is already running.
    let sock_path = ipc::socket_path();
    if sock_path.exists() {
        // Try connecting to see if it's alive.
        if tokio::net::UnixStream::connect(&sock_path).await.is_ok() {
            return Err("daemon is already running. Use `airc disconnect` first.".to_string());
        }
        // Stale socket, remove it.
        let _ = fs::remove_file(&sock_path);
    }

    if !foreground {
        // Fork into background.
        // We use a simple approach: re-exec ourselves with --foreground.
        let exe = std::env::current_exe().map_err(|e| format!("cannot find exe: {e}"))?;
        let mut cmd = process::Command::new(exe);
        cmd.arg("connect")
            .arg(&server)
            .arg("--nick")
            .arg(&nick)
            .arg("--foreground");
        if !auto_join.is_empty() {
            cmd.arg("--join").arg(auto_join.join(","));
        }
        // Detach: redirect stdio to /dev/null and don't wait.
        cmd.stdin(process::Stdio::null())
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null());
        let child = cmd.spawn().map_err(|e| format!("cannot spawn daemon: {e}"))?;
        println!("daemon started (pid {}), connecting to {server} as {nick}", child.id());
        if !auto_join.is_empty() {
            println!("auto-joining: {}", auto_join.join(", "));
        }
        return Ok(());
    }

    // --- Foreground mode: this IS the daemon. ---

    // Set up logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Write PID file.
    let pid_path = ipc::pid_path();
    let _ = fs::write(&pid_path, process::id().to_string());

    // Connect to IRC.
    let config = airc_client::ClientConfig::new(&server, &nick).with_auto_join(auto_join);

    let (client, mut event_rx) = IrcClient::connect(config)
        .await
        .map_err(|e| format!("IRC connection failed: {e}"))?;

    info!(nick = %client.nick().await, "connected to IRC, starting daemon");

    // Client-side logger (off by default, toggled via `airc log start/stop`).
    let logger: SharedLogger = Arc::new(Mutex::new(None));

    // In-memory log ring buffer (always active, for `airc logs`).
    let log_ring = new_log_ring();

    // Shutdown flag — set by `airc disconnect` to prevent auto-reconnect.
    let shutting_down: ShutdownFlag = Arc::new(AtomicBool::new(false));

    // Bind the Unix socket.
    let listener = UnixListener::bind(&sock_path)
        .map_err(|e| format!("cannot bind socket at {}: {e}", sock_path.display()))?;

    info!(path = %sock_path.display(), "listening for CLI commands");

    // Main loop: handle CLI requests and IRC events concurrently.
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            // Accept a CLI connection.
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let client = client.clone();
                        let logger = Arc::clone(&logger);
                        let log_ring = Arc::clone(&log_ring);
                        let shutting_down = Arc::clone(&shutting_down);
                        tokio::spawn(async move {
                            if let Err(e) = handle_cli_connection(stream, client, logger, log_ring, shutting_down).await {
                                error!(error = %e, "CLI connection error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "accept error");
                    }
                }
            }

            // Drain IRC events (to keep the reader task running and state updated).
            event = event_rx.recv() => {
                match event {
                    Some(IrcEvent::Disconnected { reason }) => {
                        if shutting_down.load(Ordering::Relaxed) {
                            info!(reason = %reason, "disconnected from IRC (intentional)");
                            break;
                        }
                        // Auto-reconnect is handled by the client library.
                        // Just log it and continue — Reconnecting/Reconnected
                        // events will follow.
                        info!(reason = %reason, "disconnected from IRC, auto-reconnect in progress");
                    }
                    Some(IrcEvent::Reconnecting { attempt }) => {
                        info!(attempt, "reconnecting to IRC...");
                    }
                    Some(IrcEvent::Reconnected) => {
                        info!("reconnected to IRC");
                    }
                    Some(ref ev) => {
                        // Log the event if client-side logging is active.
                        log_irc_event(&logger, &log_ring, ev);
                    }
                    None => {
                        info!("event channel closed");
                        break;
                    }
                }
            }

            // Ctrl-C (graceful shutdown).
            _ = &mut shutdown => {
                info!("received shutdown signal");
                shutting_down.store(true, Ordering::Relaxed);
                let _ = client.quit(Some("daemon shutting down")).await;
                break;
            }
        }
    }

    // Cleanup.
    drop(listener);
    let _ = fs::remove_file(&sock_path);
    let _ = fs::remove_file(&pid_path);
    info!("daemon stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Client-side event logging
// ---------------------------------------------------------------------------

/// Log an IRC event to the client-side CSV logger (if active) and to the
/// in-memory ring buffer (always active).
fn log_irc_event(logger: &SharedLogger, log_ring: &SharedLogRing, event: &IrcEvent) {
    // Build the LogEvent for the ring buffer.
    let log_event = match event {
        IrcEvent::Message(msg) => {
            Some(log_event_now(EventType::Message, &msg.target, &msg.from, &msg.text))
        }
        IrcEvent::Join { nick, channel } => {
            Some(log_event_now(EventType::Join, channel, nick, ""))
        }
        IrcEvent::Part { nick, channel, reason } => {
            Some(log_event_now(EventType::Part, channel, nick, reason.as_deref().unwrap_or("")))
        }
        IrcEvent::Quit { nick, reason } => {
            Some(log_event_now(EventType::Quit, "_quit", nick, reason.as_deref().unwrap_or("")))
        }
        IrcEvent::Kick { channel, nick, by, reason } => {
            let content = match reason {
                Some(r) => format!("by {by} ({r})"),
                None => format!("by {by}"),
            };
            Some(log_event_now(EventType::Kick, channel, nick, &content))
        }
        IrcEvent::TopicChange { channel, topic, set_by } => {
            Some(log_event_now(EventType::Topic, channel, set_by, topic))
        }
        IrcEvent::NickChange { old_nick, new_nick } => {
            Some(log_event_now(EventType::Nick, "_nick", old_nick, new_nick))
        }
        IrcEvent::Notice { from, target, text } => {
            let nick = from.as_deref().unwrap_or("server");
            Some(log_event_now(EventType::Notice, target, nick, text))
        }
        // Registered, Disconnected, Raw — not logged.
        _ => None,
    };

    // Push to ring buffer (always).
    if let Some(ref ev) = log_event {
        push_log_ring(log_ring, ev.clone());
    }

    // Write to CSV file logger (if active).
    if let Some(ref ev) = log_event {
        let guard = match logger.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(ref fl) = *guard {
            fl.log(ev);
        }
    }
}

// ---------------------------------------------------------------------------
// CLI connection handler
// ---------------------------------------------------------------------------

/// Handle a single CLI connection: read one request, execute it, send response.
async fn handle_cli_connection(
    mut stream: tokio::net::UnixStream,
    client: IrcClient,
    logger: SharedLogger,
    log_ring: SharedLogRing,
    shutting_down: ShutdownFlag,
) -> Result<(), String> {
    let (mut reader, mut writer) = stream.split();

    let req: IpcRequest = ipc::read_frame(&mut reader).await?;
    let resp = execute_request(req, &client, &logger, &log_ring, &shutting_down).await;
    ipc::write_frame(&mut writer, &resp).await?;

    Ok(())
}

/// Execute a CLI request against the IRC client.
async fn execute_request(
    req: IpcRequest,
    client: &IrcClient,
    logger: &SharedLogger,
    log_ring: &SharedLogRing,
    shutting_down: &ShutdownFlag,
) -> IpcResponse {
    let Some(command) = req.command else {
        return ipc::response_err("empty request (no command)");
    };

    match command {
        Command::Join(r) => match client.join(&r.channel).await {
            Ok(()) => ipc::response_ok(&format!("joined {}", r.channel)),
            Err(e) => ipc::response_err(&format!("join failed: {e}")),
        },

        Command::Part(r) => {
            match client.part(&r.channel, r.reason.as_deref()).await {
                Ok(()) => ipc::response_ok(&format!("left {}", r.channel)),
                Err(e) => ipc::response_err(&format!("part failed: {e}")),
            }
        }

        Command::Say(r) => match client.say(&r.target, &r.message).await {
            Ok(()) => ipc::response_ok(&format!("sent to {}", r.target)),
            Err(e) => ipc::response_err(&format!("send failed: {e}")),
        },

        Command::Fetch(r) => {
            let channel = r.channel.as_deref();
            let last = r.last.map(|n| n as usize);
            let messages = match (channel, last) {
                (Some(ch), Some(n)) => client.fetch_last(ch, n).await,
                (Some(ch), None) => client.fetch(ch).await,
                (None, Some(n)) => {
                    let all = client.fetch_all().await;
                    let start = all.len().saturating_sub(n);
                    all[start..].to_vec()
                }
                (None, None) => client.fetch_all().await,
            };
            ipc::response_messages(messages)
        }

        Command::Status(_) => {
            let channels = client.status().await;
            let nick = client.nick().await;
            ipc::response_status(nick, channels)
        }

        Command::Disconnect(_) => {
            shutting_down.store(true, Ordering::Relaxed);
            let _ = client.quit(Some("airc disconnect")).await;
            ipc::response_ok("disconnecting")
        }

        Command::LogStart(r) => {
            let dir = r
                .dir
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));

            if let Err(e) = fs::create_dir_all(&dir) {
                return ipc::response_err(&format!("cannot create log directory: {e}"));
            }

            let fl = FileLogger::new(Some(dir.clone()));
            if !fl.is_active() {
                return ipc::response_err("failed to initialize logger");
            }

            match logger.lock() {
                Ok(mut guard) => {
                    *guard = Some(fl);
                    info!(dir = %dir.display(), "client-side logging started");
                    ipc::response_ok(&format!("logging to {}", dir.display()))
                }
                Err(_) => ipc::response_err("internal error: logger lock poisoned"),
            }
        }

        Command::LogStop(_) => match logger.lock() {
            Ok(mut guard) => {
                let was_active = guard.is_some();
                *guard = None;
                if was_active {
                    info!("client-side logging stopped");
                    ipc::response_ok("logging stopped")
                } else {
                    ipc::response_ok("logging was not active")
                }
            }
            Err(_) => ipc::response_err("internal error: logger lock poisoned"),
        },

        Command::Logs(r) => {
            let last = r.last.map(|n| n as usize);
            let channel = r.channel.as_deref();
            let events = read_log_ring(log_ring, last, channel);
            ipc::response_logs(events)
        },
    }
}
