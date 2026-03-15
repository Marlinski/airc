//! IRC command dispatch — handles all commands for registered clients.
//!
//! The top-level [`handle_command`] function dispatches to focused submodules:
//!
//! | Module          | Commands                                          |
//! |-----------------|---------------------------------------------------|
//! | `nick`          | NICK                                              |
//! | `channel`       | JOIN, PART, NAMES, LIST, KICK, INVITE             |
//! | `message`       | PRIVMSG, NOTICE                                   |
//! | `mode`          | MODE                                              |
//! | `user`          | QUIT, PING, AWAY, ISON, WHO, WHOIS                |
//! | `server`        | MOTD, VERSION, LUSERS, OPER, KILL                 |

pub mod channel;
pub mod message;
pub mod mode;
pub mod nick;
pub mod server;
pub mod user;

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};

use crate::client::{ClientHandle, ClientId};
use crate::state::SharedState;

// Re-export the public API used by connection.rs / server.rs.
pub use server::{send_lusers, send_motd};

/// Dispatch a parsed IRC command from a registered client.
pub async fn handle_command(state: &SharedState, client_id: ClientId, msg: &IrcMessage) {
    match &msg.command {
        Command::Nick => nick::handle_nick(state, client_id, msg).await,
        Command::Join => channel::handle_join(state, client_id, msg).await,
        Command::Part => channel::handle_part(state, client_id, msg).await,
        Command::Privmsg => message::handle_privmsg(state, client_id, msg).await,
        Command::Notice => message::handle_notice(state, client_id, msg).await,
        Command::Quit => user::handle_quit(state, client_id, msg).await,
        Command::Ping => user::handle_ping(state, client_id, msg).await,
        Command::Pong => {} // silently accept
        Command::Topic => channel::handle_topic(state, client_id, msg).await,
        Command::Mode => mode::handle_mode(state, client_id, msg).await,
        Command::Who => user::handle_who(state, client_id, msg).await,
        Command::Whois => user::handle_whois(state, client_id, msg).await,
        Command::List => channel::handle_list(state, client_id, msg).await,
        Command::Names => channel::handle_names(state, client_id, msg).await,
        Command::Kick => channel::handle_kick(state, client_id, msg).await,
        Command::Invite => channel::handle_invite(state, client_id, msg).await,
        Command::Away => user::handle_away(state, client_id, msg).await,
        Command::Ison => user::handle_ison(state, client_id, msg).await,
        Command::Silence => {
            // SILENCE is handled client-side via NickServ, not by the server.
            if let Some(client) = state.get_client(client_id).await {
                client.send_numeric(ERR_UNKNOWNCOMMAND, &["SILENCE", "Unknown command"]);
            }
        }
        Command::Motd => server::handle_motd(state, client_id).await,
        Command::Version => server::handle_version(state, client_id).await,
        Command::Unknown(cmd_str) if cmd_str.eq_ignore_ascii_case("LUSERS") => {
            server::handle_lusers(state, client_id).await
        }
        Command::Oper => server::handle_oper(state, client_id, msg).await,
        Command::Kill => server::handle_kill(state, client_id, msg).await,
        Command::User | Command::Pass => {
            if let Some(client) = state.get_client(client_id).await {
                client.send_numeric(ERR_ALREADYREGISTERED, &["You may not reregister"]);
            }
        }
        _ => {
            if let Some(client) = state.get_client(client_id).await {
                let cmd_str = msg.command.to_string();
                client.send_numeric(ERR_UNKNOWNCOMMAND, &[&cmd_str, "Unknown command"]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (used by multiple submodules)
// ---------------------------------------------------------------------------

/// Whether a string looks like a channel name (`#` or `&` prefix).
pub(super) fn is_channel_name(s: &str) -> bool {
    airc_shared::validate::is_channel_name(s)
}

/// Send topic info (or "no topic") to a single client.
pub(super) fn send_topic_to_client(
    client: &ClientHandle,
    channel_name: &str,
    topic: &Option<(String, String, u64)>,
) {
    match topic {
        Some((text, setter, timestamp)) => {
            client.send_numeric(RPL_TOPIC, &[channel_name, text]);
            client.send_numeric(
                RPL_TOPICWHOTIME,
                &[channel_name, setter, &timestamp.to_string()],
            );
        }
        None => {
            client.send_numeric(RPL_NOTOPIC, &[channel_name, "No topic is set"]);
        }
    }
}

/// Send the NAMES list for a channel to a single client.
pub(super) async fn send_names_to_client(
    state: &SharedState,
    client: &ClientHandle,
    channel_name: &str,
) {
    if let Some(nicks) = state.channel_nicks_with_prefix(channel_name).await {
        let names_str = nicks.join(" ");
        // Channel type: @ for secret, = for public.
        let chan_type = if state.channel_is_secret(channel_name).await {
            "@"
        } else {
            "="
        };
        // RPL_NAMREPLY: <type> <channel> :<nicks>
        client.send_numeric(RPL_NAMREPLY, &[chan_type, channel_name, &names_str]);
    }
    client.send_numeric(RPL_ENDOFNAMES, &[channel_name, "End of /NAMES list"]);
}
