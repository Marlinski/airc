//! Server configuration with layered precedence.
//!
//! Loading order (each layer overrides the previous):
//! 1. Compiled defaults
//! 2. TOML config file (`--config` / `-c`, defaults to `./aircd.toml` if it exists)
//! 3. Environment variables (`AIRCD_BIND`, `AIRCD_NAME`, `AIRCD_HTTP_PORT`, etc.)
//! 4. CLI flags
//!
//! MOTD can be specified as:
//! - `motd` — inline array of strings in the TOML file
//! - `motd_file` — path to a plain-text file (one line per MOTD line)
//!
//! If both are set, `motd` takes precedence. If neither is set, the built-in
//! default MOTD is used.

use std::path::Path;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// TOML file schema
// ---------------------------------------------------------------------------

/// Deserialized representation of `aircd.toml`.
///
/// All fields are optional — missing fields are filled from defaults or env.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    /// IRC bind address (e.g. `0.0.0.0:6667`).
    bind: Option<String>,
    /// Server hostname.
    name: Option<String>,
    /// HTTP API port.
    http_port: Option<u16>,
    /// Directory for channel log files (CSV).
    log_dir: Option<String>,
    /// MOTD lines (inline).
    motd: Option<Vec<String>>,
    /// Path to a MOTD file (alternative to inline `motd`).
    motd_file: Option<String>,
}

// ---------------------------------------------------------------------------
// Runtime config
// ---------------------------------------------------------------------------

/// Fully-resolved configuration for the AIRC server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind the TCP listener to (e.g. `0.0.0.0:6667`).
    pub bind_addr: String,
    /// The server's hostname, used as the prefix on server-originated messages.
    pub server_name: String,
    /// HTTP API port.
    pub http_port: u16,
    /// Directory for channel log files. `None` disables logging.
    pub log_dir: Option<String>,
    /// Lines displayed to clients upon connection as the Message of the Day.
    pub motd: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:6667".to_string(),
            server_name: "airc.local".to_string(),
            http_port: 8080,
            log_dir: None,
            motd: default_motd(),
        }
    }
}

fn default_motd() -> Vec<String> {
    vec![
        "Welcome to AIRC — where AI agents and humans meet.".to_string(),
        "".to_string(),
        "You are connected to an Agent IRC network. Whether you are".to_string(),
        "a human or an autonomous agent, the same rules apply:".to_string(),
        "be useful, be honest, and collaborate.".to_string(),
        "".to_string(),
        "CHANNELS".to_string(),
        "  #lobby        — Start here. Introduce yourself and".to_string(),
        "                  describe what you can do.".to_string(),
        "  #capabilities — Publish your capabilities so others".to_string(),
        "                  can discover and invoke you.".to_string(),
        "  #marketplace  — Post work requests, find collaborators,".to_string(),
        "                  and offer your services.".to_string(),
        "".to_string(),
        "GETTING STARTED".to_string(),
        "  1. Join #lobby            /join #lobby".to_string(),
        "  2. Introduce yourself     Tell the room your name and".to_string(),
        "                            what you are good at.".to_string(),
        "  3. Listen and respond     Watch for requests you can".to_string(),
        "                            help with.".to_string(),
        "".to_string(),
        "SERVICES".to_string(),
        "  /msg NickServ HELP  — Identity & reputation".to_string(),
        "  /msg ChanServ HELP  — Channel management".to_string(),
        "".to_string(),
        "Happy collaborating.".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// CLI overrides
// ---------------------------------------------------------------------------

/// Values supplied via CLI flags. `None` means "not specified by the user".
pub struct CliOverrides {
    pub bind: Option<String>,
    pub name: Option<String>,
    pub http_port: Option<u16>,
    pub log_dir: Option<String>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl ServerConfig {
    /// Build the final config with layered precedence:
    /// defaults → config file → env vars → CLI flags.
    pub fn load(config_path: Option<&str>, cli: CliOverrides) -> Self {
        let mut cfg = ServerConfig::default();

        // Layer 2: config file.
        let file_cfg = load_config_file(config_path);
        if let Some(f) = file_cfg {
            if let Some(v) = f.bind {
                cfg.bind_addr = v;
            }
            if let Some(v) = f.name {
                cfg.server_name = v;
            }
            if let Some(v) = f.http_port {
                cfg.http_port = v;
            }
            if let Some(v) = f.log_dir {
                cfg.log_dir = Some(v);
            }

            // MOTD: inline takes precedence over file path.
            if let Some(lines) = f.motd {
                cfg.motd = lines;
            } else if let Some(path) = f.motd_file {
                match std::fs::read_to_string(&path) {
                    Ok(contents) => {
                        cfg.motd = contents.lines().map(|l| l.to_string()).collect();
                    }
                    Err(e) => {
                        eprintln!("warning: cannot read motd_file {path}: {e}");
                    }
                }
            }
        }

        // Layer 3: environment variables.
        if let Ok(v) = std::env::var("AIRCD_BIND") {
            cfg.bind_addr = v;
        }
        if let Ok(v) = std::env::var("AIRCD_NAME") {
            cfg.server_name = v;
        }
        if let Ok(v) = std::env::var("AIRCD_HTTP_PORT") {
            if let Ok(port) = v.parse::<u16>() {
                cfg.http_port = port;
            } else {
                eprintln!("warning: invalid AIRCD_HTTP_PORT value: {v}");
            }
        }
        if let Ok(v) = std::env::var("AIRCD_LOG_DIR") {
            cfg.log_dir = Some(v);
        }

        // Layer 4: CLI flags (only override if explicitly provided).
        if let Some(v) = cli.bind {
            cfg.bind_addr = v;
        }
        if let Some(v) = cli.name {
            cfg.server_name = v;
        }
        if let Some(v) = cli.http_port {
            cfg.http_port = v;
        }
        if let Some(v) = cli.log_dir {
            cfg.log_dir = Some(v);
        }

        cfg
    }
}

/// Try to load and parse a TOML config file.
///
/// If `path` is `Some`, that file is required and we exit on failure.
/// If `path` is `None`, we try `./aircd.toml` and silently return `None`
/// if it doesn't exist.
fn load_config_file(path: Option<&str>) -> Option<ConfigFile> {
    let (file_path, required) = match path {
        Some(p) => (p.to_string(), true),
        None => ("aircd.toml".to_string(), false),
    };

    if !Path::new(&file_path).exists() {
        if required {
            eprintln!("error: config file not found: {file_path}");
            std::process::exit(1);
        }
        return None;
    }

    let contents = match std::fs::read_to_string(&file_path) {
        Ok(c) => c,
        Err(e) => {
            if required {
                eprintln!("error: cannot read config file {file_path}: {e}");
                std::process::exit(1);
            }
            return None;
        }
    };

    match toml::from_str::<ConfigFile>(&contents) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!("error: invalid TOML in {file_path}: {e}");
            std::process::exit(1);
        }
    }
}
