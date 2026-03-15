//! `aircd` — AIRC server daemon.
//!
//! A standards-compliant IRC server where AI agents and humans meet,
//! discover capabilities, earn reputation, and collaborate.
//!
//! This binary both **runs** the server and **controls** it:
//! - `aircd start`              — launch the server in the background
//! - `aircd start --foreground` — run the server in the current process
//! - `aircd stop`               — graceful shutdown via IPC, or `-f` for SIGKILL
//! - `aircd status`             — query server stats via IPC

mod channel;
mod client;
mod config;
mod connection;
mod handler;
mod ipc;
mod logger;
mod relay;
mod server;
mod state;
mod web;

#[cfg(test)]
mod relay_tests;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::{Parser, Subcommand};
use prost::Message;

use airc_shared::aircd_ipc;

use crate::config::{CliOverrides, ServerConfig};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "aircd", about = "AIRC server daemon", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the IRC server.
    Start {
        /// Path to TOML config file (defaults to ./aircd.toml if it exists).
        #[arg(short, long)]
        config: Option<String>,

        /// IRC bind address.
        #[arg(long)]
        bind: Option<String>,

        /// Server hostname.
        #[arg(long)]
        name: Option<String>,

        /// HTTP API port.
        #[arg(long)]
        http_port: Option<u16>,

        /// Run in foreground (don't daemonize).
        #[arg(long)]
        foreground: bool,

        /// Directory for channel log files (CSV). Logging is disabled if omitted.
        #[arg(long)]
        log_dir: Option<String>,

        /// Path to PEM certificate file for TLS.
        #[arg(long)]
        tls_cert: Option<String>,

        /// Path to PEM private key file for TLS.
        #[arg(long)]
        tls_key: Option<String>,

        /// TLS bind address (default 0.0.0.0:6697).
        #[arg(long)]
        tls_bind: Option<String>,
    },

    /// Stop the running server.
    Stop {
        /// Force kill via PID (skip graceful IPC shutdown).
        #[arg(short, long)]
        force: bool,
    },

    /// Show server statistics.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn pid_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("aircd.pid")
}

fn log_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("aircd.log")
}

/// Path to the aircd IPC Unix socket (controller side).
///
/// Must match [`ipc::socket_path()`] used by the server side.
fn ipc_socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("aircd.sock")
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_start(cfg: ServerConfig, foreground: bool) {
    if foreground {
        // When re-exec'd with --foreground, the parent already checked for
        // conflicts and wrote the PID file.  Skip straight to running.
        run_server_foreground(cfg);
        return;
    }

    // Check if already running.
    let pid_file = pid_path();
    if let Some(pid) = read_pid(&pid_file) {
        if is_process_alive(pid) {
            eprintln!("server is already running (pid {pid}). Use `aircd stop` first.");
            std::process::exit(1);
        }
        // Stale PID file.
        let _ = fs::remove_file(&pid_file);
    }

    // Pre-flight: check that required ports are free.
    check_port_available(&cfg.bind_addr, "IRC");
    check_port_available(&format!("0.0.0.0:{}", cfg.http_port), "HTTP API");
    if cfg.tls_enabled() {
        check_port_available(cfg.tls_bind_addr(), "IRC TLS");
    }

    // Daemonize: re-exec ourselves with --foreground.
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("cannot determine own executable path: {e}");
        std::process::exit(1);
    });

    let log_file_path = log_path();
    let log_file = fs::File::create(&log_file_path).unwrap_or_else(|e| {
        eprintln!("cannot create log file {}: {e}", log_file_path.display());
        std::process::exit(1);
    });
    let log_stderr = log_file.try_clone().unwrap();

    // Re-exec with explicit flags so the child inherits the resolved config
    // (the child won't re-read the config file or env vars).
    let child = {
        let mut cmd = Command::new(&exe);
        cmd.arg("start")
            .arg("--foreground")
            .arg("--bind")
            .arg(&cfg.bind_addr)
            .arg("--name")
            .arg(&cfg.server_name)
            .arg("--http-port")
            .arg(cfg.http_port.to_string());
        if let Some(ref dir) = cfg.log_dir {
            cmd.arg("--log-dir").arg(dir);
        }
        if let Some(ref cert) = cfg.tls_cert {
            cmd.arg("--tls-cert").arg(cert);
        }
        if let Some(ref key) = cfg.tls_key {
            cmd.arg("--tls-key").arg(key);
        }
        if let Some(ref bind) = cfg.tls_bind {
            cmd.arg("--tls-bind").arg(bind);
        }
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or("info".to_string()),
        )
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_stderr)
        .spawn()
    };

    match child {
        Ok(child) => {
            let pid = child.id();
            fs::write(&pid_file, pid.to_string()).unwrap_or_else(|e| {
                eprintln!("warning: cannot write PID file: {e}");
            });
            println!("server started (pid {pid})");
            println!("  irc:  {}", cfg.bind_addr);
            if cfg.tls_enabled() {
                println!("  tls:  {}", cfg.tls_bind_addr());
            }
            println!("  http: 0.0.0.0:{}", cfg.http_port);
            println!("  ws:   ws://0.0.0.0:{}/ws", cfg.http_port);
            println!("  log:  {}", log_file_path.display());

            // Wait briefly and check it's still alive.
            std::thread::sleep(Duration::from_millis(500));
            if !is_process_alive(pid) {
                eprintln!("\nserver exited immediately. Check log:");
                print_log_tail(&log_file_path, 20);
                let _ = fs::remove_file(&pid_file);
                std::process::exit(1);
            }

            // Verify the IPC socket is reachable (server is ready).
            if !wait_for_ipc(10) {
                eprintln!("\nwarning: server is running but IPC socket is not responding");
                eprintln!("recent log output:");
                print_log_tail(&log_file_path, 20);
                // Kill the half-working server.
                kill_pid(pid, false);
                let _ = fs::remove_file(&pid_file);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("failed to start server: {e}");
            std::process::exit(1);
        }
    }
}

/// Run the AIRC server in the current process (foreground mode).
fn run_server_foreground(cfg: ServerConfig) {
    use std::sync::Arc;
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Build TLS acceptor from config (if TLS is configured).
    let tls_acceptor = cfg.tls_acceptor();

    let http_port = cfg.http_port;

    // Construct the relay backend (NoopRelay for single-instance mode).
    let relay: Arc<dyn relay::Relay> = Arc::new(relay::NoopRelay::new());

    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| {
        eprintln!("failed to create tokio runtime: {e}");
        std::process::exit(1);
    });

    rt.block_on(async {
        let state = state::SharedState::new(cfg, relay);
        state.create_default_channels().await;

        // Start HTTP API server.
        let http_addr = format!("0.0.0.0:{http_port}");
        let http_state = state.clone();
        let http_handle = tokio::spawn(async move {
            if let Err(e) = web::serve(&http_addr, http_state).await {
                tracing::error!(error = %e, "HTTP server failed");
            }
        });

        // Start IRC server (blocks until shutdown signal).
        let srv = server::Server::new(state, tls_acceptor);
        if let Err(e) = srv.run().await {
            tracing::error!(error = %e, "IRC server failed");
            std::process::exit(1);
        }

        // IRC server returned (ctrl-c or IPC shutdown), abort the HTTP server.
        http_handle.abort();
    });
}

fn cmd_stop(force: bool) {
    let pid_file = pid_path();
    let sock_path = ipc_socket_path();

    match read_pid(&pid_file) {
        Some(pid) => {
            if !is_process_alive(pid) {
                println!("server is not running (stale pid {pid})");
                let _ = fs::remove_file(&pid_file);
                let _ = fs::remove_file(&sock_path);
                return;
            }

            if force {
                // Skip IPC, go straight to SIGKILL.
                println!("force-killing server (pid {pid})...");
                kill_pid(pid, true);
                let _ = fs::remove_file(&pid_file);
                let _ = fs::remove_file(&sock_path);
                println!("server killed (pid {pid})");
                return;
            }

            // Try graceful IPC shutdown first.
            if sock_path.exists() {
                let req = aircd_ipc::AircdRequest {
                    command: Some(aircd_ipc::aircd_request::Command::Shutdown(
                        aircd_ipc::ShutdownRequest {
                            reason: Some("aircd stop".to_string()),
                        },
                    )),
                };
                match ipc_request(&sock_path, &req) {
                    Ok(resp) if resp.ok => {
                        let msg = match resp.payload {
                            Some(aircd_ipc::aircd_response::Payload::Shutdown(s)) => s.message,
                            _ => "shutdown acknowledged".to_string(),
                        };
                        println!("{msg}");
                        // Wait for process to exit.
                        for _ in 0..20 {
                            std::thread::sleep(Duration::from_millis(250));
                            if !is_process_alive(pid) {
                                break;
                            }
                        }
                        if !is_process_alive(pid) {
                            let _ = fs::remove_file(&pid_file);
                            let _ = fs::remove_file(&sock_path);
                            println!("server stopped (pid {pid})");
                            return;
                        }
                        eprintln!(
                            "server did not exit after graceful shutdown, sending SIGTERM..."
                        );
                    }
                    Ok(resp) => {
                        let err = resp.error.unwrap_or_else(|| "unknown error".to_string());
                        eprintln!("IPC shutdown failed ({err}), falling back to SIGTERM...");
                    }
                    Err(e) => {
                        eprintln!("IPC shutdown failed ({e}), falling back to SIGTERM...");
                    }
                }
            }

            // Fall back to SIGTERM.
            kill_pid(pid, false);

            // Wait for it to exit.
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(250));
                if !is_process_alive(pid) {
                    break;
                }
            }
            if is_process_alive(pid) {
                eprintln!("server (pid {pid}) did not exit, sending SIGKILL");
                kill_pid(pid, true);
            }
            let _ = fs::remove_file(&pid_file);
            let _ = fs::remove_file(&sock_path);
            println!("server stopped (pid {pid})");
        }
        None => {
            println!("server is not running (no PID file)");
        }
    }
}

fn cmd_status(json: bool) {
    // Check PID.
    let pid_file = pid_path();
    let pid = read_pid(&pid_file);
    let running = pid.map(is_process_alive).unwrap_or(false);

    if !running {
        if json {
            println!(r#"{{"running": false}}"#);
        } else {
            println!("server is not running");
        }
        return;
    }

    // Fetch stats via IPC.
    let sock_path = ipc_socket_path();
    let req = aircd_ipc::AircdRequest {
        command: Some(aircd_ipc::aircd_request::Command::Stats(
            aircd_ipc::StatsRequest {},
        )),
    };

    let stats = match ipc_request(&sock_path, &req) {
        Ok(resp) if resp.ok => match resp.payload {
            Some(aircd_ipc::aircd_response::Payload::Stats(s)) => Some(s),
            _ => None,
        },
        Ok(resp) => {
            let err = resp.error.unwrap_or_else(|| "unknown error".to_string());
            eprintln!("IPC stats request failed: {err}");
            None
        }
        Err(e) => {
            eprintln!("cannot reach server via IPC: {e}");
            None
        }
    };

    if json {
        let mut out = serde_json::Map::new();
        out.insert("running".into(), serde_json::Value::Bool(true));
        if let Some(ref p) = pid {
            out.insert("pid".into(), serde_json::json!(p));
        }
        if let Some(ref s) = stats {
            out.insert("server_name".into(), serde_json::json!(s.server_name));
            out.insert("users_online".into(), serde_json::json!(s.users_online));
            out.insert(
                "channels_active".into(),
                serde_json::json!(s.channels_active),
            );
            out.insert("uptime_seconds".into(), serde_json::json!(s.uptime_seconds));
            out.insert(
                "channels".into(),
                serde_json::to_value(&s.channels).unwrap_or_default(),
            );
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }

    // Human-readable output.
    if let Some(ref p) = pid {
        println!("server running (pid {p})");
    }
    if let Some(ref s) = stats {
        println!("  name:     {}", s.server_name);
        println!("  users:    {}", s.users_online);
        println!("  channels: {}", s.channels_active);
        println!("  uptime:   {}", format_duration(s.uptime_seconds));

        if !s.channels.is_empty() {
            println!();
            for ch in &s.channels {
                let topic = ch.topic.as_deref().unwrap_or("(no topic)");
                println!(
                    "  {:<20} {:>3} users  {}  {}",
                    ch.name, ch.member_count, ch.modes, topic
                );
            }
        }
    } else {
        println!("  (could not reach server via IPC)");
    }
}

// ---------------------------------------------------------------------------
// IPC helper (synchronous — controller commands don't need async)
// ---------------------------------------------------------------------------

/// Send a request to the running server over the IPC Unix socket and read the
/// response. Uses length-prefixed protobuf frames.
fn ipc_request(
    sock_path: &PathBuf,
    req: &aircd_ipc::AircdRequest,
) -> Result<aircd_ipc::AircdResponse, String> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(sock_path).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set timeout: {e}"))?;

    // Write length-prefixed frame.
    let buf = req.encode_to_vec();
    let len = buf.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| format!("write length: {e}"))?;
    stream
        .write_all(&buf)
        .map_err(|e| format!("write payload: {e}"))?;

    // Read response: length-prefixed frame.
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read length: {e}"))?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    if resp_len > 16 * 1024 * 1024 {
        return Err(format!("response frame too large: {resp_len}"));
    }
    let mut resp_buf = vec![0u8; resp_len];
    stream
        .read_exact(&mut resp_buf)
        .map_err(|e| format!("read payload: {e}"))?;

    aircd_ipc::AircdResponse::decode(&resp_buf[..]).map_err(|e| format!("decode: {e}"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Send a signal to a process.
fn kill_pid(pid: u32, force: bool) {
    #[cfg(unix)]
    {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, force);
        eprintln!("stop is only supported on Unix");
        std::process::exit(1);
    }
}

/// Check that a port is available before starting the server.
/// Exits with an error message if the port is already in use.
fn check_port_available(addr: &str, label: &str) {
    use std::net::TcpListener;
    match TcpListener::bind(addr) {
        Ok(_) => {} // Port is free; the listener drops and releases it immediately.
        Err(e) => {
            eprintln!("{label} port {addr} is already in use: {e}");
            eprintln!("free the port or choose a different one.");
            std::process::exit(1);
        }
    }
}

/// Wait for the IPC socket to become reachable, retrying up to `max_attempts` times.
fn wait_for_ipc(max_attempts: u32) -> bool {
    let sock_path = ipc_socket_path();
    let req = aircd_ipc::AircdRequest {
        command: Some(aircd_ipc::aircd_request::Command::Stats(
            aircd_ipc::StatsRequest {},
        )),
    };
    for _ in 0..max_attempts {
        std::thread::sleep(Duration::from_millis(250));
        if ipc_request(&sock_path, &req).is_ok() {
            return true;
        }
    }
    false
}

/// Print the last N lines of a log file to stderr.
fn print_log_tail(path: &PathBuf, n: usize) {
    if let Ok(f) = fs::File::open(path) {
        let lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();
        let start = lines.len().saturating_sub(n);
        for line in &lines[start..] {
            eprintln!("  {line}");
        }
    }
}

/// Read a PID from a file, returning None if the file doesn't exist or is invalid.
fn read_pid(path: &PathBuf) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Check if a process is alive (Unix only).
fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0) checks if the process exists without sending a signal.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Format seconds into a human-readable duration.
fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            config: config_path,
            bind,
            name,
            http_port,
            foreground,
            log_dir,
            tls_cert,
            tls_key,
            tls_bind,
        } => {
            let cfg = ServerConfig::load(
                config_path.as_deref(),
                CliOverrides {
                    bind,
                    name,
                    http_port,
                    log_dir,
                    tls_cert,
                    tls_key,
                    tls_bind,
                },
            );
            cmd_start(cfg, foreground);
        }
        Commands::Stop { force } => cmd_stop(force),
        Commands::Status { json } => cmd_status(json),
    }
}
