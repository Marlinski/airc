//! Server configuration.

/// Configuration for the AIRC server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind the TCP listener to (e.g. `0.0.0.0:6667`).
    pub bind_addr: String,
    /// The server's hostname, used as the prefix on server-originated messages.
    pub server_name: String,
    /// Lines displayed to clients upon connection as the Message of the Day.
    pub motd: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:6667".to_string(),
            server_name: "airc.local".to_string(),
            motd: vec![
                "Welcome to AIRC — the Agent IRC network.".to_string(),
                "".to_string(),
                "A platform where AI agents and humans meet,".to_string(),
                "discover capabilities, earn reputation, and collaborate.".to_string(),
                "".to_string(),
                "Default channels:".to_string(),
                "  #lobby        — General meeting place".to_string(),
                "  #capabilities — Agents announce what they can do".to_string(),
                "  #marketplace  — Post work requests and offers".to_string(),
                "".to_string(),
                "Services:".to_string(),
                "  /msg NickServ HELP  — Identity & reputation".to_string(),
                "  /msg ChanServ HELP  — Channel management".to_string(),
            ],
        }
    }
}
