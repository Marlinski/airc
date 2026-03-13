//! Low-level IRC connection: TCP reader/writer tasks.
//!
//! Handles splitting a TCP stream into a line reader and a line writer,
//! automatic PONG responses, and dispatching parsed messages to the state
//! tracker.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use airc_shared::reply::*;
use airc_shared::validate::is_channel_name;
use airc_shared::{Command, IrcMessage, Prefix};

use crate::config::ClientConfig;
use crate::error::ClientError;
use crate::event::{IrcEvent, MessageKind, new_channel_message};
use crate::state::ClientState;

/// Sender for outgoing raw IRC lines (without \r\n).
pub type LineSender = mpsc::Sender<String>;

/// Receiver for high-level IRC events.
pub type EventReceiver = mpsc::Receiver<IrcEvent>;

/// Establish a TCP connection and spawn reader/writer tasks.
///
/// Returns:
/// - A `LineSender` for sending raw IRC lines
/// - An `EventReceiver` for receiving parsed events
/// - The `ClientState` for querying buffered data
pub async fn connect(
    config: &ClientConfig,
) -> Result<(LineSender, EventReceiver, ClientState), ClientError> {
    info!(addr = %config.server_addr, nick = %config.nick, "connecting to IRC server");

    let stream = TcpStream::connect(&config.server_addr).await?;
    let (reader, writer) = stream.into_split();

    let state = ClientState::new(config.nick.clone(), config.buffer_size);

    // Channel for outgoing lines: caller -> writer task.
    let (line_tx, line_rx) = mpsc::channel::<String>(512);

    // Channel for parsed events: reader task -> caller.
    let (event_tx, event_rx) = mpsc::channel::<IrcEvent>(512);

    // Spawn writer task.
    let writer_tx = line_tx.clone();
    tokio::spawn(write_loop(writer, line_rx));

    // Spawn reader task.
    let reader_state = state.clone();
    let reader_line_tx = writer_tx.clone();
    tokio::spawn(read_loop(reader, reader_line_tx, event_tx, reader_state));

    // Send registration sequence.
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

/// Writer task: drains the line channel and writes to the TCP socket.
async fn write_loop(mut writer: tokio::net::tcp::OwnedWriteHalf, mut rx: mpsc::Receiver<String>) {
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

/// Reader task: reads lines from the TCP socket, parses them, updates state,
/// and emits events.
async fn read_loop(
    reader: tokio::net::tcp::OwnedReadHalf,
    line_tx: LineSender,
    event_tx: mpsc::Sender<IrcEvent>,
    state: ClientState,
) {
    let mut buf_reader = BufReader::new(reader);
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        match buf_reader.read_line(&mut line_buf).await {
            Ok(0) => {
                // Connection closed.
                info!("connection closed by server");
                let _ = event_tx
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
                        handle_message(msg, &line_tx, &event_tx, &state).await;
                    }
                    Err(e) => {
                        warn!(error = %e, line = %trimmed, "failed to parse IRC message");
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "read error");
                let _ = event_tx
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

/// Process a single parsed IRC message: update state, auto-respond, emit events.
async fn handle_message(
    msg: IrcMessage,
    line_tx: &LineSender,
    event_tx: &mpsc::Sender<IrcEvent>,
    state: &ClientState,
) {
    match &msg.command {
        // -- Auto PONG --------------------------------------------------------
        Command::Ping => {
            let token = msg.params.first().map(|s| s.as_str()).unwrap_or("");
            let _ = line_tx.send(IrcMessage::pong(token).serialize()).await;
        }

        // -- Registration complete --------------------------------------------
        Command::Numeric(n) if *n == RPL_WELCOME => {
            let nick = msg.params.first().cloned().unwrap_or_default();
            let server = msg.prefix.clone().unwrap_or_default();
            let message = msg.params.last().cloned().unwrap_or_default();
            state.set_nick(nick.clone()).await;
            state.set_server_name(server.clone()).await;
            state.set_registered().await;
            let _ = event_tx
                .send(IrcEvent::Registered {
                    nick,
                    server,
                    message,
                })
                .await;
        }

        // -- Nick in use ------------------------------------------------------
        Command::Numeric(n) if *n == ERR_NICKNAMEINUSE => {
            // Try appending underscore.
            let current = state.nick().await;
            let new_nick = format!("{current}_");
            state.set_nick(new_nick.clone()).await;
            let _ = line_tx.send(IrcMessage::nick(&new_nick).serialize()).await;
            warn!(nick = %new_nick, "nick in use, trying alternative");
        }

        // -- Topic (332) -----------------------------------------------------
        Command::Numeric(n) if *n == RPL_TOPIC => {
            if msg.params.len() >= 3 {
                let channel = &msg.params[1];
                let topic = &msg.params[2];
                state.set_topic(channel, topic.clone()).await;
            }
        }

        // -- NAMES reply (353) ------------------------------------------------
        Command::Numeric(n) if *n == RPL_NAMREPLY => {
            // params: nick = #channel :nick1 @nick2 +nick3
            if msg.params.len() >= 4 {
                let channel = &msg.params[2];
                let names_str = &msg.params[3];
                let members: Vec<String> = names_str
                    .split_whitespace()
                    .map(|n| {
                        // Strip mode prefixes (@, +, %)
                        n.trim_start_matches(|c| c == '@' || c == '+' || c == '%')
                            .to_string()
                    })
                    .collect();
                state.set_members(channel, members).await;
            }
        }

        // -- JOIN -------------------------------------------------------------
        Command::Join => {
            let channel = msg.params.first().cloned().unwrap_or_default();
            let nick = extract_nick(&msg.prefix);
            let our_nick = state.nick().await;

            if nick.eq_ignore_ascii_case(&our_nick) {
                // We joined.
                state.join_channel(&channel).await;
            } else {
                // Someone else joined.
                state.add_member(&channel, &nick).await;
            }
            let _ = event_tx.send(IrcEvent::Join { nick, channel }).await;
        }

        // -- PART -------------------------------------------------------------
        Command::Part => {
            let channel = msg.params.first().cloned().unwrap_or_default();
            let reason = msg.params.get(1).cloned();
            let nick = extract_nick(&msg.prefix);
            let our_nick = state.nick().await;

            if nick.eq_ignore_ascii_case(&our_nick) {
                state.part_channel(&channel).await;
            } else {
                state.remove_member(&channel, &nick).await;
            }
            let _ = event_tx
                .send(IrcEvent::Part {
                    nick,
                    channel,
                    reason,
                })
                .await;
        }

        // -- QUIT -------------------------------------------------------------
        Command::Quit => {
            let nick = extract_nick(&msg.prefix);
            let reason = msg.params.first().cloned();
            state.remove_member_all(&nick).await;
            let _ = event_tx.send(IrcEvent::Quit { nick, reason }).await;
        }

        // -- KICK -------------------------------------------------------------
        Command::Kick => {
            if msg.params.len() >= 2 {
                let channel = msg.params[0].clone();
                let kicked = msg.params[1].clone();
                let reason = msg.params.get(2).cloned();
                let by = extract_nick(&msg.prefix);
                let our_nick = state.nick().await;

                if kicked.eq_ignore_ascii_case(&our_nick) {
                    state.part_channel(&channel).await;
                } else {
                    state.remove_member(&channel, &kicked).await;
                }
                let _ = event_tx
                    .send(IrcEvent::Kick {
                        channel,
                        nick: kicked,
                        by,
                        reason,
                    })
                    .await;
            }
        }

        // -- NICK change ------------------------------------------------------
        Command::Nick => {
            let old_nick = extract_nick(&msg.prefix);
            let new_nick = msg.params.first().cloned().unwrap_or_default();
            let our_nick = state.nick().await;

            if old_nick.eq_ignore_ascii_case(&our_nick) {
                state.set_nick(new_nick.clone()).await;
            }
            state.rename_member(&old_nick, &new_nick).await;
            let _ = event_tx
                .send(IrcEvent::NickChange { old_nick, new_nick })
                .await;
        }

        // -- TOPIC change -----------------------------------------------------
        Command::Topic => {
            let channel = msg.params.first().cloned().unwrap_or_default();
            let topic = msg.params.get(1).cloned().unwrap_or_default();
            let set_by = extract_nick(&msg.prefix);
            state.set_topic(&channel, topic.clone()).await;
            let _ = event_tx
                .send(IrcEvent::TopicChange {
                    channel,
                    topic,
                    set_by,
                })
                .await;
        }

        // -- PRIVMSG ----------------------------------------------------------
        Command::Privmsg => {
            if msg.params.len() >= 2 {
                let target = &msg.params[0];
                let text = &msg.params[1];
                let from = extract_nick(&msg.prefix);

                // Detect CTCP ACTION.
                let (text, kind) = if text.starts_with("\x01ACTION ") && text.ends_with('\x01') {
                    let inner = &text[8..text.len() - 1];
                    (inner.to_string(), MessageKind::Action)
                } else {
                    (text.clone(), MessageKind::Normal)
                };

                let cm = new_channel_message(target.clone(), from.clone(), text.clone(), kind);

                if is_channel_name(target) {
                    state.push_message(target, cm).await;
                } else {
                    // Private message — buffer under the sender's nick.
                    state.push_private_message(cm).await;
                }

                let _ = event_tx
                    .send(IrcEvent::Message(new_channel_message(
                        target.clone(),
                        from,
                        text,
                        MessageKind::Normal,
                    )))
                    .await;
            }
        }

        // -- NOTICE -----------------------------------------------------------
        Command::Notice => {
            let target = msg.params.first().cloned().unwrap_or_default();
            let text = msg.params.get(1).cloned().unwrap_or_default();
            let from = msg.prefix.as_ref().map(|p| extract_nick(&Some(p.clone())));

            // Buffer notices from service bots as messages too.
            if let Some(ref from_nick) = from {
                let cm = new_channel_message(
                    target.clone(),
                    from_nick.clone(),
                    text.clone(),
                    MessageKind::Normal,
                );
                if is_channel_name(&target) {
                    state.push_message(&target, cm).await;
                } else {
                    state.push_private_message(cm).await;
                }
            }

            let _ = event_tx.send(IrcEvent::Notice { from, target, text }).await;
        }

        // -- MOTD (375, 372, 376) --------------------------------------------
        Command::Numeric(n) if *n == RPL_MOTDSTART => {
            // 375 — start of MOTD. Nothing to emit; the body lines follow.
        }
        Command::Numeric(n) if *n == RPL_MOTD => {
            // 372 — a single MOTD body line.
            // params: <nick> :<motd line>
            let line = msg.params.last().cloned().unwrap_or_default();
            // Strip the conventional "- " prefix that IRC servers prepend.
            let line = line.strip_prefix("- ").unwrap_or(&line).to_string();
            let _ = event_tx.send(IrcEvent::Motd { line }).await;
        }
        Command::Numeric(n) if *n == RPL_ENDOFMOTD => {
            // 376 — end of MOTD.
            let _ = event_tx.send(IrcEvent::MotdEnd).await;
        }

        // -- Everything else: emit as Raw ------------------------------------
        _ => {
            let _ = event_tx
                .send(IrcEvent::Raw {
                    line: msg.serialize(),
                })
                .await;
        }
    }
}

/// Extract the nick from an optional prefix string.
fn extract_nick(prefix: &Option<String>) -> String {
    match prefix {
        Some(p) => {
            let parsed = Prefix::parse(p);
            parsed.nick().to_string()
        }
        None => String::new(),
    }
}
