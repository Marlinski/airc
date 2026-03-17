//! Low-level IRC connection: TCP/TLS transport tasks.
//!
//! Handles establishing a TCP (or TLS) connection, splitting it into a
//! line reader and a line writer, automatic PONG responses, and delegating
//! all parsed messages to the `handler` module.
//!
//! # CAP / SASL negotiation
//!
//! The client always sends `CAP LS 302` before NICK/USER so that capability
//! negotiation is possible.  The flow is:
//!
//! ```text
//! C: CAP LS 302
//! C: NICK <nick>
//! C: USER <user> 0 * :<realname>
//!
//! S: CAP * LS :sasl multi-prefix ...   ← server capabilities
//!
//! If sasl is in the cap list and SaslConfig is set:
//!   C: CAP REQ :sasl
//!   S: CAP * ACK :sasl
//!   C: AUTHENTICATE <MECHANISM>
//!   S: AUTHENTICATE +               ← empty challenge (PLAIN)
//!   C: AUTHENTICATE <base64-payload>
//!   S: 900 ...                       ← RPL_LOGGEDIN
//!   S: 903 ...                       ← RPL_SASLSUCCESS
//! Else:
//!   (no sasl negotiation)
//!
//! C: CAP END
//! S: 001 ...                         ← RPL_WELCOME (registration complete)
//! ```
//!
//! Protocol logic lives in `handler/`; this module is pure transport.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_rustls::TlsConnector;
use tracing::{debug, error, info, warn};

use airc_shared::IrcMessage;

use crate::config::{ClientConfig, TlsMode};
use crate::error::ClientError;
use crate::event::IrcEvent;
use crate::handler::cap::SaslHandshake;
use crate::handler::{ConnContext, handle_message};
use crate::state::ClientState;

/// Sender for outgoing raw IRC lines (without \r\n).
pub type LineSender = mpsc::Sender<String>;

/// Receiver for high-level IRC events.
pub type EventReceiver = mpsc::Receiver<IrcEvent>;

// ---------------------------------------------------------------------------
// TLS helpers
// ---------------------------------------------------------------------------

/// Build a `rustls` TLS connector using system root certificates.
fn tls_connector() -> TlsConnector {
    let root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(tls_config))
}

/// Extract the hostname from a `host:port` address string.
fn extract_host(addr: &str) -> &str {
    // Handle [IPv6]:port
    if let Some(bracket_end) = addr.find(']') {
        return &addr[..=bracket_end];
    }
    // host:port
    addr.rsplit_once(':').map_or(addr, |(host, _)| host)
}

/// Establish a TLS connection over TCP.
async fn establish_tls(
    addr: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, ClientError> {
    let tcp_stream = TcpStream::connect(addr).await?;
    let host = extract_host(addr);
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let connector = tls_connector();
    let tls_stream = connector
        .connect(server_name, tcp_stream)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;

    info!(addr = %addr, "TLS connection established");
    Ok(tls_stream)
}

/// Derive the plain-text fallback address (replace port with 6667).
fn fallback_plain_addr(addr: &str) -> String {
    if let Some(bracket_end) = addr.find(']') {
        if addr[bracket_end..].contains(':') {
            return format!("{}:6667", &addr[..=bracket_end]);
        }
        return format!("{addr}:6667");
    }
    match addr.rsplit_once(':') {
        Some((host, _port)) => format!("{host}:6667"),
        None => format!("{addr}:6667"),
    }
}

// ---------------------------------------------------------------------------
// Public connect entry point
// ---------------------------------------------------------------------------

/// Establish a connection (TLS or plain TCP) and spawn reader/writer tasks.
///
/// Returns:
/// - A `LineSender` for sending raw IRC lines
/// - An `EventReceiver` for receiving parsed events
/// - The `ClientState` for querying buffered data
pub async fn connect(
    config: &ClientConfig,
) -> Result<(LineSender, EventReceiver, ClientState), ClientError> {
    info!(addr = %config.server_addr, nick = %config.nick, tls = ?config.tls, "connecting to IRC server");

    let state = ClientState::new(config.nick.clone(), config.buffer_size);

    // Channel for outgoing lines: caller -> writer task.
    let (line_tx, line_rx) = mpsc::channel::<String>(512);

    // Channel for parsed events: reader task -> caller.
    let (event_tx, event_rx) = mpsc::channel::<IrcEvent>(512);

    // Shared SASL handshake state — only populated when sasl is configured.
    let sasl_state: Arc<Mutex<Option<SaslHandshake>>> =
        Arc::new(Mutex::new(config.sasl.as_ref().map(|s| SaslHandshake {
            account: s.account.clone(),
            password: s.password.clone(),
            mechanism: s.mechanism,
            step: crate::handler::cap::SaslStep::AwaitingCapAck,
        })));

    match config.tls {
        TlsMode::Required => {
            let tls_stream = establish_tls(&config.server_addr).await?;
            let (reader, writer) = tokio::io::split(tls_stream);
            spawn_io_tasks(
                reader,
                writer,
                line_tx.clone(),
                line_rx,
                event_tx,
                state.clone(),
                sasl_state,
            );
        }
        TlsMode::Preferred => match establish_tls(&config.server_addr).await {
            Ok(tls_stream) => {
                let (reader, writer) = tokio::io::split(tls_stream);
                spawn_io_tasks(
                    reader,
                    writer,
                    line_tx.clone(),
                    line_rx,
                    event_tx,
                    state.clone(),
                    sasl_state,
                );
            }
            Err(tls_err) => {
                info!(error = %tls_err, "TLS connection failed, falling back to plain TCP");
                let plain_addr = fallback_plain_addr(&config.server_addr);
                let tcp_stream = TcpStream::connect(&plain_addr).await?;
                info!(addr = %plain_addr, "plain TCP connection established (fallback)");
                let (reader, writer) = tokio::io::split(tcp_stream);
                spawn_io_tasks(
                    reader,
                    writer,
                    line_tx.clone(),
                    line_rx,
                    event_tx,
                    state.clone(),
                    sasl_state,
                );
            }
        },
        TlsMode::Disabled => {
            let tcp_stream = TcpStream::connect(&config.server_addr).await?;
            info!(addr = %config.server_addr, "plain TCP connection established");
            let (reader, writer) = tokio::io::split(tcp_stream);
            spawn_io_tasks(
                reader,
                writer,
                line_tx.clone(),
                line_rx,
                event_tx,
                state.clone(),
                sasl_state,
            );
        }
    }

    // Send registration sequence.
    //
    // Always start with CAP LS 302 so the server knows we speak IRCv3.
    // NICK and USER follow immediately — the server buffers them until
    // CAP END completes registration.
    let _ = line_tx.send("CAP LS 302".to_string()).await;

    if let Some(ref pass) = config.password {
        let _ = line_tx.send(IrcMessage::pass(pass).serialize()).await;
    }
    let _ = line_tx
        .send(IrcMessage::nick(&config.nick).serialize())
        .await;
    let _ = line_tx
        .send(IrcMessage::user(&config.username, &config.realname).serialize())
        .await;

    Ok((line_tx, event_rx, state))
}

// ---------------------------------------------------------------------------
// I/O task spawning
// ---------------------------------------------------------------------------

fn spawn_io_tasks<R, W>(
    reader: R,
    writer: W,
    line_tx: LineSender,
    line_rx: mpsc::Receiver<String>,
    event_tx: mpsc::Sender<IrcEvent>,
    state: ClientState,
    sasl_state: Arc<Mutex<Option<SaslHandshake>>>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(write_loop(writer, line_rx));
    tokio::spawn(read_loop(reader, line_tx, event_tx, state, sasl_state));
}

/// Writer task: drains the line channel and writes to the stream.
async fn write_loop<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::Receiver<String>) {
    while let Some(line) = rx.recv().await {
        debug!(line = %line, "-> sending");
        let data = format!("{line}\r\n");
        if let Err(e) = writer.write_all(data.as_bytes()).await {
            error!(error = %e, "write error, stopping writer");
            break;
        }
    }
    debug!("writer task exiting");
}

/// Reader task: reads lines from the stream, parses them, and delegates to
/// `handler::handle_message`.
async fn read_loop<R: AsyncRead + Unpin>(
    reader: R,
    line_tx: LineSender,
    event_tx: mpsc::Sender<IrcEvent>,
    state: ClientState,
    sasl_state: Arc<Mutex<Option<SaslHandshake>>>,
) {
    let mut buf_reader = BufReader::new(reader);
    let mut line_buf = String::new();

    let ctx = ConnContext {
        line_tx,
        event_tx: event_tx.clone(),
        state,
        sasl_state,
    };

    loop {
        line_buf.clear();
        match buf_reader.read_line(&mut line_buf).await {
            Ok(0) => {
                info!("connection closed by server");
                let _ = ctx
                    .event_tx
                    .send(IrcEvent::Disconnected {
                        reason: "connection closed by server".to_string(),
                    })
                    .await;
                break;
            }
            Ok(_) => {
                let trimmed = line_buf.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                debug!(line = %trimmed, "<- received");
                match IrcMessage::parse(trimmed) {
                    Ok(msg) => {
                        handle_message(msg, &ctx).await;
                    }
                    Err(e) => {
                        warn!(error = %e, line = %trimmed, "failed to parse IRC message");
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "read error");
                let _ = ctx
                    .event_tx
                    .send(IrcEvent::Disconnected {
                        reason: format!("read error: {e}"),
                    })
                    .await;
                break;
            }
        }
    }
    debug!("reader task exiting");
}
