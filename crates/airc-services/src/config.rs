//! Configuration for airc-services (external bot framework).
//!
//! NickServ and ChanServ are now embedded in aircd — this config only covers
//! the connection parameters used by external service bots.
//!
//! Loading order (each layer overrides the previous):
//! 1. Compiled defaults
//! 2. TOML config file (`--config` / `-c`)
//! 3. Environment variables (`AIRC_SERVICES_` prefix)
//! 4. CLI flags

use std::path::Path;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// TOML file schema
// ---------------------------------------------------------------------------

/// Deserialized representation of the services TOML config.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    /// IRC server address (e.g. `localhost:6667`).
    server: Option<String>,
    /// Connection password (PASS), if required by the server.
    server_password: Option<String>,
    /// TLS mode: "required", "preferred", "disabled".
    tls: Option<String>,
    /// Operator name for OPER authentication.
    oper_name: Option<String>,
    /// Operator password for OPER authentication.
    oper_password: Option<String>,
    /// Directory for persistence files.
    data_dir: Option<String>,
}

// ---------------------------------------------------------------------------
// Runtime config
// ---------------------------------------------------------------------------

/// Fully-resolved configuration for airc-services.
#[derive(Debug, Clone)]
pub struct ServicesConfig {
    /// IRC server address in `host:port` format.
    pub server_addr: String,
    /// Connection password (PASS), if required by the server.
    pub server_password: Option<String>,
    /// TLS mode for the IRC connection.
    pub tls: airc_client::TlsMode,
    /// Operator name for OPER authentication.
    pub oper_name: String,
    /// Operator password for OPER authentication.
    pub oper_password: String,
    /// Directory for persistence files.
    pub data_dir: String,
}

impl Default for ServicesConfig {
    fn default() -> Self {
        Self {
            server_addr: "localhost:6667".to_string(),
            server_password: None,
            tls: airc_client::TlsMode::Disabled,
            oper_name: "services".to_string(),
            oper_password: String::new(),
            data_dir: ".".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// CLI overrides
// ---------------------------------------------------------------------------

/// Values supplied via CLI flags. `None` means "not specified".
pub struct CliOverrides {
    pub server: Option<String>,
    pub oper_name: Option<String>,
    pub oper_password: Option<String>,
    pub data_dir: Option<String>,
    pub tls: Option<String>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl ServicesConfig {
    /// Build the final config with layered precedence:
    /// defaults → config file → env vars → CLI flags.
    pub fn load(config_path: Option<&str>, cli: CliOverrides) -> Self {
        let mut cfg = ServicesConfig::default();

        // Layer 2: config file.
        if let Some(f) = load_config_file(config_path) {
            if let Some(v) = f.server {
                cfg.server_addr = v;
            }
            if let Some(v) = f.server_password {
                cfg.server_password = Some(v);
            }
            if let Some(v) = f.tls {
                cfg.tls = parse_tls_mode(&v);
            }
            if let Some(v) = f.oper_name {
                cfg.oper_name = v;
            }
            if let Some(v) = f.oper_password {
                cfg.oper_password = v;
            }
            if let Some(v) = f.data_dir {
                cfg.data_dir = v;
            }
        }

        // Layer 3: environment variables.
        if let Ok(v) = std::env::var("AIRC_SERVICES_SERVER") {
            cfg.server_addr = v;
        }
        if let Ok(v) = std::env::var("AIRC_SERVICES_TLS") {
            cfg.tls = parse_tls_mode(&v);
        }
        if let Ok(v) = std::env::var("AIRC_SERVICES_OPER_NAME") {
            cfg.oper_name = v;
        }
        if let Ok(v) = std::env::var("AIRC_SERVICES_OPER_PASSWORD") {
            cfg.oper_password = v;
        }
        if let Ok(v) = std::env::var("AIRC_SERVICES_DATA_DIR") {
            cfg.data_dir = v;
        }

        // Layer 4: CLI flags.
        if let Some(v) = cli.server {
            cfg.server_addr = v;
        }
        if let Some(v) = cli.tls {
            cfg.tls = parse_tls_mode(&v);
        }
        if let Some(v) = cli.oper_name {
            cfg.oper_name = v;
        }
        if let Some(v) = cli.oper_password {
            cfg.oper_password = v;
        }
        if let Some(v) = cli.data_dir {
            cfg.data_dir = v;
        }

        cfg
    }
}

fn parse_tls_mode(s: &str) -> airc_client::TlsMode {
    match s.to_ascii_lowercase().as_str() {
        "required" | "true" | "yes" | "1" => airc_client::TlsMode::Required,
        "preferred" | "auto" => airc_client::TlsMode::Preferred,
        "disabled" | "false" | "no" | "0" | "none" => airc_client::TlsMode::Disabled,
        _ => {
            eprintln!("warning: unknown TLS mode '{s}', defaulting to disabled");
            airc_client::TlsMode::Disabled
        }
    }
}

/// Try to load and parse a TOML config file.
fn load_config_file(path: Option<&str>) -> Option<ConfigFile> {
    let (file_path, required) = match path {
        Some(p) => (p.to_string(), true),
        None => ("airc-services.toml".to_string(), false),
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
