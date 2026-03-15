//! TCP accept loop — the heart of the AIRC server.

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

use crate::connection::Connection;
use crate::ipc;
use crate::state::SharedState;

/// The AIRC IRC server.
pub struct Server {
    state: SharedState,
    tls_acceptor: Option<TlsAcceptor>,
}

impl Server {
    pub fn new(state: SharedState, tls_acceptor: Option<TlsAcceptor>) -> Self {
        Self {
            state,
            tls_acceptor,
        }
    }

    /// Bind and run the server. This function runs until the process is shut down.
    pub async fn run(&self) -> std::io::Result<()> {
        let addr = &self.state.config().bind_addr;
        let listener = TcpListener::bind(addr).await?;

        info!(addr = %addr, name = %self.state.server_name(), "AIRC server listening (plaintext)");

        // Optionally bind the TLS listener.
        let tls_listener = if self.tls_acceptor.is_some() {
            let tls_addr = self.state.config().tls_bind_addr().to_string();
            let tls_listener = TcpListener::bind(&tls_addr).await?;
            info!(addr = %tls_addr, "AIRC server listening (TLS)");
            Some(tls_listener)
        } else {
            None
        };

        // Start the IPC listener for `aircd stop` / `aircd status` commands.
        let (mut ipc_rx, ipc_sock_path) = match ipc::start_listener(self.state.clone()) {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "failed to start IPC listener (aircd stop will not work)");
                // Continue without IPC — the server still works, just no
                // graceful shutdown via `aircd stop`.
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                drop(tx); // closed immediately — ipc_rx will never yield
                (rx, ipc::socket_path())
            }
        };

        let result = async {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, peer_addr)) => {
                                // Disable Nagle's algorithm for low-latency delivery.
                                let _ = stream.set_nodelay(true);

                                let id = self.state.next_client_id();
                                let hostname = peer_addr.ip().to_string();
                                info!(client_id = %id, peer = %peer_addr, "new connection");

                                let conn = Connection::new(id, self.state.clone(), hostname);
                                tokio::spawn(async move {
                                    conn.run_tcp(stream).await;
                                });
                            }
                            Err(e) => {
                                error!(error = %e, "failed to accept connection");
                            }
                        }
                    }

                    // TLS accept (only active if TLS is configured).
                    result = async {
                        match (&tls_listener, &self.tls_acceptor) {
                            (Some(l), Some(_)) => l.accept().await,
                            _ => std::future::pending().await,
                        }
                    } => {
                        match result {
                            Ok((stream, peer_addr)) => {
                                // Disable Nagle's algorithm for low-latency delivery.
                                let _ = stream.set_nodelay(true);

                                let acceptor = self.tls_acceptor.clone().unwrap();
                                let id = self.state.next_client_id();
                                let hostname = peer_addr.ip().to_string();
                                info!(client_id = %id, peer = %peer_addr, "new TLS connection");

                                let state = self.state.clone();
                                tokio::spawn(async move {
                                    match acceptor.accept(stream).await {
                                        Ok(tls_stream) => {
                                            let conn = Connection::new(id, state, hostname);
                                            conn.run_tls(tls_stream).await;
                                        }
                                        Err(e) => {
                                            warn!(client_id = %id, error = %e, "TLS handshake failed");
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                error!(error = %e, "failed to accept TLS connection");
                            }
                        }
                    }

                    // Shutdown signal from IPC (aircd stop).
                    Some(signal) = ipc_rx.recv() => {
                        match signal {
                            ipc::IpcSignal::Shutdown { reason } => {
                                info!(reason = %reason, "shutting down via IPC");
                                self.state.shutdown_all().await;
                                info!("server shut down gracefully (IPC)");
                                return Ok(());
                            }
                        }
                    }

                    _ = tokio::signal::ctrl_c() => {
                        info!("received shutdown signal, closing connections...");
                        self.state.shutdown_all().await;
                        info!("server shut down gracefully");
                        return Ok(());
                    }
                }
            }
        }
        .await;

        // Clean up IPC socket.
        ipc::cleanup(&ipc_sock_path);

        result
    }
}
