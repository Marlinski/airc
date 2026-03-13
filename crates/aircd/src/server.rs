//! TCP accept loop — the heart of the AIRC server.

use tokio::net::TcpListener;
use tracing::{error, info};

use crate::connection::Connection;
use crate::ipc;
use crate::state::SharedState;

/// The AIRC IRC server.
pub struct Server {
    state: SharedState,
}

impl Server {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    /// Bind and run the server. This function runs until the process is shut down.
    pub async fn run(&self) -> std::io::Result<()> {
        let addr = &self.state.config().bind_addr;
        let listener = TcpListener::bind(addr).await?;

        info!(addr = %addr, name = %self.state.server_name(), "AIRC server listening");

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
                                let id = self.state.next_client_id();
                                let hostname = peer_addr.ip().to_string();
                                info!(client_id = %id, peer = %peer_addr, "new connection");

                                let conn = Connection::new(id, self.state.clone(), hostname);
                                tokio::spawn(async move {
                                    conn.run(stream).await;
                                });
                            }
                            Err(e) => {
                                error!(error = %e, "failed to accept connection");
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
