use std::io::Write;
use std::net::SocketAddr;

use tokio::net::TcpListener;

use crate::catalogue::Catalogue;
use crate::config::{EffectiveConfiguration, ServeOverrides};
use crate::error::AppError;
use crate::runtime_lock::DaemonLock;
use crate::web::{ApplicationState, router};

pub async fn serve(overrides: ServeOverrides) -> Result<(), AppError> {
    let _lock = DaemonLock::acquire()?;
    let configuration = EffectiveConfiguration::load(&overrides)?;
    let storage = configuration.storage.path.clone();
    let catalogue = tokio::task::spawn_blocking(move || Catalogue::open_data_root(&storage))
        .await
        .map_err(|error| AppError::internal(format!("catalogue worker failed: {error}")))??;
    let address = configuration
        .server
        .listen
        .parse::<SocketAddr>()
        .map_err(|_| AppError::usage("server.listen must be a socket address"))?;
    let listener = TcpListener::bind(address)
        .await
        .map_err(|error| AppError::unavailable_with(format!("cannot bind {address}: {error}")))?;
    let state = ApplicationState::new(configuration, catalogue.clone());
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| AppError::internal(format!("HTTP server failed: {error}")))?;
    tokio::task::spawn_blocking(move || catalogue.checkpoint())
        .await
        .map_err(|error| AppError::internal(format!("checkpoint worker failed: {error}")))??;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut terminate), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            loop {
                tokio::select! {
                    _ = terminate.recv() => return,
                    _ = tokio::signal::ctrl_c() => return,
                    _ = hangup.recv() => {
                        let mut stderr = std::io::stderr().lock();
                        let _ = writeln!(
                            stderr,
                            "SIGHUP reload is unsupported; restart Observatory to apply configuration"
                        );
                    },
                }
            }
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
