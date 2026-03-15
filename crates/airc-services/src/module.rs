//! Module system for IRC services.
//!
//! Each service (NickServ, ChanServ, etc.) is composed of modules — logically
//! grouped features that can be toggled on/off in the TOML configuration.
//!
//! Modules implement the [`ServiceModule`] trait and are dispatched through a
//! [`ServiceDispatcher`] that iterates modules until one handles a command.

use std::future::Future;
use std::pin::Pin;

use airc_client::IrcClient;
use tracing::warn;

// ---------------------------------------------------------------------------
// CommandContext — bundles everything a module needs to handle a command
// ---------------------------------------------------------------------------

/// Context passed to a module's `handle` method.
///
/// This decouples modules from the IRC client directly — they receive the
/// parsed command and a reply handle without knowing about `IrcClient`.
pub struct CommandContext<'a> {
    /// Nick of the user who sent the command.
    pub sender: &'a str,
    /// The uppercased command name (e.g. "REGISTER", "VOUCH").
    pub command: &'a str,
    /// First argument after the command, if any.
    pub arg1: Option<&'a str>,
    /// Everything after the second token, if any.
    pub arg2: Option<&'a str>,
    /// The raw text after the command name (unparsed arguments).
    pub raw_args: &'a str,
    /// Handle for sending NOTICE replies back to the user.
    reply: ReplyHandle,
}

impl<'a> CommandContext<'a> {
    /// Send a NOTICE reply to the command sender.
    pub async fn reply(&self, text: &str) {
        self.reply.notice(self.sender, text).await;
    }
}

/// Parse a raw PRIVMSG text into a `CommandContext`.
///
/// Splits the text into command + up to 2 arguments (matching the existing
/// `splitn(3, ' ')` pattern used by both NickServ and ChanServ).
pub fn parse_command<'a>(sender: &'a str, text: &'a str, client: &IrcClient) -> CommandContext<'a> {
    let parts: Vec<&str> = text.splitn(3, ' ').collect();
    let command_str = parts.first().copied().unwrap_or("");
    let arg1 = parts.get(1).copied();
    let arg2 = parts.get(2).copied();

    // raw_args is everything after the first token (the command).
    let raw_args = text
        .get(command_str.len()..)
        .map(|s| s.trim_start())
        .unwrap_or("");

    CommandContext {
        sender,
        command: command_str, // Note: caller should uppercase this before dispatch
        arg1,
        arg2,
        raw_args,
        reply: ReplyHandle {
            client: client.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// ReplyHandle — wraps IrcClient for sending NOTICE replies
// ---------------------------------------------------------------------------

/// Lightweight handle for sending NOTICE messages back to a user.
#[derive(Clone)]
pub struct ReplyHandle {
    client: IrcClient,
}

impl ReplyHandle {
    /// Send a NOTICE to the given target.
    pub async fn notice(&self, target: &str, text: &str) {
        if let Err(e) = self.client.notice(target, text).await {
            warn!(error = %e, target = %target, "failed to send reply");
        }
    }
}

// ---------------------------------------------------------------------------
// ServiceModule trait — the unit of modularity
// ---------------------------------------------------------------------------

/// A module within a service (e.g. NickServ's "identity" module).
///
/// Modules are compile-time units with config-driven feature flags, not
/// runtime-loadable plugins. Each module declares which commands it handles,
/// and the dispatcher iterates modules until one claims the command.
pub trait ServiceModule: Send + Sync {
    /// Human-readable name for this module (e.g. "identity", "keypair").
    #[allow(dead_code)]
    fn name(&self) -> &str;

    /// List of command names (uppercase) this module handles.
    #[allow(dead_code)]
    fn commands(&self) -> &[&str];

    /// Try to handle a command. Returns `true` if this module handled it.
    ///
    /// Uses a boxed future for dyn-compatibility. Implementors should use
    /// `Box::pin(async move { ... })` in their implementation.
    fn handle<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

    /// Help lines for this module, shown when the user sends HELP.
    fn help_lines(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// ServiceDispatcher — routes commands to the right module
// ---------------------------------------------------------------------------

/// Holds a set of modules and dispatches commands to the first one that
/// claims it. Also aggregates HELP output from all active modules.
pub struct ServiceDispatcher {
    /// The service name (e.g. "NickServ"), used in HELP header.
    service_name: String,
    /// Active modules, in dispatch order.
    modules: Vec<Box<dyn ServiceModule>>,
    /// Reply handle for sending messages (used for HELP and unknown-command).
    reply: ReplyHandle,
}

impl ServiceDispatcher {
    /// Create a new dispatcher with the given service name and modules.
    pub fn new(
        service_name: String,
        modules: Vec<Box<dyn ServiceModule>>,
        client: &IrcClient,
    ) -> Self {
        Self {
            service_name,
            modules,
            reply: ReplyHandle {
                client: client.clone(),
            },
        }
    }

    /// Dispatch a raw PRIVMSG to the appropriate module.
    ///
    /// Parses the text, uppercases the command, checks for HELP, then
    /// iterates modules. If no module handles it, sends an "unknown command"
    /// reply.
    pub async fn dispatch(&self, sender: &str, text: &str, client: &IrcClient) {
        let mut ctx = parse_command(sender, text, client);

        // Uppercase the command for case-insensitive matching.
        // We need to store the uppercased version and point ctx.command to it.
        let command_upper = ctx.command.to_ascii_uppercase();

        // Handle HELP specially — aggregate from all modules.
        if command_upper == "HELP" {
            self.send_help(sender).await;
            return;
        }

        // Re-assign command to the uppercased version.
        ctx.command = &command_upper;

        // Try each module in order.
        for module in &self.modules {
            if module.handle(&ctx).await {
                return;
            }
        }

        // No module handled it.
        self.reply
            .notice(
                sender,
                &format!("Unknown command: {command_upper}. Use HELP for a list of commands."),
            )
            .await;
    }

    /// Send aggregated HELP output from all active modules.
    async fn send_help(&self, sender: &str) {
        self.reply
            .notice(sender, &format!("{} commands:", self.service_name))
            .await;
        for module in &self.modules {
            for line in module.help_lines() {
                self.reply.notice(sender, &line).await;
            }
        }
    }
}
