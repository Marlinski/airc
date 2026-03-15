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
//!
//! ## TLS
//!
//! TLS is optional. Supply `tls_cert` and `tls_key` (PEM files) to enable it.
//! The server will listen on `tls_bind` (default `0.0.0.0:6697`) in addition
//! to the plain-text IRC port.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use tokio_rustls::TlsAcceptor;

// ---------------------------------------------------------------------------
// TOML file schema
// ---------------------------------------------------------------------------

/// An IRC operator entry.
///
/// Configured in TOML as:
/// ```toml
/// [[operators]]
/// name = "services"
/// password = "secret"
/// service = true
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct OperatorEntry {
    /// Operator name (used as the first parameter to `OPER`).
    pub name: String,
    /// Plain-text password (used as the second parameter to `OPER`).
    pub password: String,
    /// If `true`, the user receives `+S` (service) mode in addition to `+o`.
    #[serde(default)]
    pub service: bool,
}

/// Relay backend configuration.
///
/// Configured in TOML as:
/// ```toml
/// [relay]
/// backend = "none"   # or "redis" in the future
/// redis_url = "redis://127.0.0.1:6379"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// Backend type: `"none"` (default, single-instance) or `"redis"` (future).
    pub backend: String,
    /// Redis URL for the `redis` backend.
    pub redis_url: Option<String>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            backend: "none".to_string(),
            redis_url: None,
        }
    }
}

/// Services (NickServ / ChanServ) configuration embedded in `aircd.toml`.
///
/// Configured in TOML as:
/// ```toml
/// [services]
/// data_dir = "data/services"
///
/// [services.nickserv]
/// enabled = true
/// enforce_registered = false
///
/// [services.chanserv]
/// enabled = true
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServicesConfig {
    pub nickserv: NickServConfig,
    pub chanserv: ChanServConfig,
    pub data_dir: PathBuf,
}

impl Default for ServicesConfig {
    fn default() -> Self {
        Self {
            nickserv: NickServConfig::default(),
            chanserv: ChanServConfig::default(),
            data_dir: PathBuf::from("data/services"),
        }
    }
}

/// NickServ-specific settings within `[services.nickserv]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NickServConfig {
    pub enabled: bool,
    /// Kick unregistered users from channels that require identification.
    pub enforce_registered: bool,
}

impl Default for NickServConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enforce_registered: false,
        }
    }
}

/// ChanServ-specific settings within `[services.chanserv]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChanServConfig {
    pub enabled: bool,
}

impl Default for ChanServConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

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
    /// Path to PEM certificate file for TLS.
    tls_cert: Option<String>,
    /// Path to PEM private key file for TLS.
    tls_key: Option<String>,
    /// TLS bind address (default `0.0.0.0:6697`).
    tls_bind: Option<String>,
    /// IRC operator accounts.
    #[serde(default)]
    operators: Vec<OperatorEntry>,
    /// Relay backend configuration.
    #[serde(default)]
    relay: RelayConfig,
    /// Services (NickServ / ChanServ) configuration.
    #[serde(default)]
    services: ServicesConfig,
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
    /// Path to PEM certificate file for TLS. `None` disables TLS.
    pub tls_cert: Option<String>,
    /// Path to PEM private key file for TLS.
    pub tls_key: Option<String>,
    /// Address to bind the TLS listener to (e.g. `0.0.0.0:6697`).
    pub tls_bind: Option<String>,
    /// IRC operator accounts.
    pub operators: Vec<OperatorEntry>,
    /// Relay backend configuration.
    pub relay: RelayConfig,
    /// Services (NickServ / ChanServ) configuration.
    pub services: ServicesConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:6667".to_string(),
            server_name: "airc.local".to_string(),
            http_port: 8080,
            log_dir: None,
            motd: default_motd(),
            tls_cert: None,
            tls_key: None,
            tls_bind: None,
            operators: Vec::new(),
            relay: RelayConfig::default(),
            services: ServicesConfig::default(),
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
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub tls_bind: Option<String>,
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
            if let Some(v) = f.tls_cert {
                cfg.tls_cert = Some(v);
            }
            if let Some(v) = f.tls_key {
                cfg.tls_key = Some(v);
            }
            if let Some(v) = f.tls_bind {
                cfg.tls_bind = Some(v);
            }

            // Operators (only from config file — no env/CLI override for lists).
            if !f.operators.is_empty() {
                cfg.operators = f.operators;
            }

            // Relay configuration.
            cfg.relay = f.relay;

            // Services configuration.
            cfg.services = f.services;

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
        if let Ok(v) = std::env::var("AIRCD_TLS_CERT") {
            cfg.tls_cert = Some(v);
        }
        if let Ok(v) = std::env::var("AIRCD_TLS_KEY") {
            cfg.tls_key = Some(v);
        }
        if let Ok(v) = std::env::var("AIRCD_TLS_BIND") {
            cfg.tls_bind = Some(v);
        }
        if let Ok(v) = std::env::var("AIRCD_RELAY_BACKEND") {
            cfg.relay.backend = v;
        }
        if let Ok(v) = std::env::var("AIRCD_RELAY_REDIS_URL") {
            cfg.relay.redis_url = Some(v);
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
        if let Some(v) = cli.tls_cert {
            cfg.tls_cert = Some(v);
        }
        if let Some(v) = cli.tls_key {
            cfg.tls_key = Some(v);
        }
        if let Some(v) = cli.tls_bind {
            cfg.tls_bind = Some(v);
        }

        cfg
    }

    /// Returns `true` if TLS is configured (both cert and key are set).
    pub fn tls_enabled(&self) -> bool {
        self.tls_cert.is_some() && self.tls_key.is_some()
    }

    /// Returns the TLS bind address, defaulting to `0.0.0.0:6697`.
    pub fn tls_bind_addr(&self) -> &str {
        self.tls_bind.as_deref().unwrap_or("0.0.0.0:6697")
    }

    /// Build a [`TlsAcceptor`] from the configured cert and key files.
    ///
    /// Returns `None` if TLS is not configured. Exits with an error if the
    /// cert/key files cannot be read or parsed.
    pub fn tls_acceptor(&self) -> Option<TlsAcceptor> {
        let (cert_path, key_path) = match (&self.tls_cert, &self.tls_key) {
            (Some(c), Some(k)) => (c, k),
            (Some(_), None) => {
                eprintln!("error: tls_cert is set but tls_key is missing");
                std::process::exit(1);
            }
            (None, Some(_)) => {
                eprintln!("error: tls_key is set but tls_cert is missing");
                std::process::exit(1);
            }
            (None, None) => return None,
        };

        let certs = load_certs(cert_path);
        let key = load_private_key(key_path);

        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap_or_else(|e| {
                eprintln!("error: invalid TLS configuration: {e}");
                std::process::exit(1);
            });

        Some(TlsAcceptor::from(Arc::new(config)))
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

// ---------------------------------------------------------------------------
// TLS helpers
// ---------------------------------------------------------------------------

/// Load PEM-encoded certificates from a file.
fn load_certs(path: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    let file = std::fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("error: cannot open TLS certificate file {path}: {e}");
        std::process::exit(1);
    });
    let mut reader = std::io::BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| {
            eprintln!("error: cannot parse TLS certificate file {path}: {e}");
            std::process::exit(1);
        })
}

/// Load the first PEM-encoded private key from a file.
fn load_private_key(path: &str) -> rustls::pki_types::PrivateKeyDer<'static> {
    let file = std::fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("error: cannot open TLS key file {path}: {e}");
        std::process::exit(1);
    });
    let mut reader = std::io::BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .unwrap_or_else(|e| {
            eprintln!("error: cannot parse TLS key file {path}: {e}");
            std::process::exit(1);
        })
        .unwrap_or_else(|| {
            eprintln!("error: no private key found in {path}");
            std::process::exit(1);
        })
}
