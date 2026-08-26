use configgymajiggy::{Config, PinStore, router};
use log::{debug, error, info};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::MissedTickBehavior;

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    dotenvy::dotenv().ok();
    env_logger::init();

    let config = Arc::new(Config::from_env()?);

    if std::env::args().any(|arg| arg == "--health-check") {
        return health_check(&config).await;
    }

    let store = PinStore::new(Arc::clone(&config));

    let sweeper = tokio::spawn(supervised_sweeper(store.clone(), config.cleanup_interval));

    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    info!("Server running on http://{}", config.bind_address);

    axum::serve(listener, router(store))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    sweeper.abort();
    info!("Shutdown complete");
    Ok(())
}

/// Runs the expiry sweep forever, restarting it if it ever panics. Losing this
/// task silently would let memory grow to `MAX_ENTRIES` and stay there while the
/// service still reported healthy.
async fn supervised_sweeper(store: PinStore, interval: Duration) {
    loop {
        let worker = tokio::spawn(sweep_loop(store.clone(), interval));

        match worker.await {
            Ok(()) => return,
            Err(e) if e.is_cancelled() => return,
            Err(e) => error!("expiry sweeper died ({e}); restarting"),
        }
    }
}

async fn sweep_loop(store: PinStore, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // Burst would turn one slow sweep into back-to-back full-map scans.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        let evicted = store.sweep();
        if evicted > 0 {
            // Never the key: it carries the namespace, which is this service's
            // only access control.
            debug!("swept {evicted} stale pins");
        }
    }
}

async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install SIGINT handler");
    };

    // SIGTERM is what `docker compose down` sends.
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
}

/// Probes `/ready` over a raw socket so the runtime image does not need curl.
/// Readiness rather than liveness, so Docker restarts a container whose expiry
/// sweeper has died instead of leaving it to fill up.
async fn health_check(config: &Config) -> Result<(), BoxError> {
    let port = config.port().ok_or("BIND_ADDRESS has no port")?;

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;

        let mut response = vec![0u8; 64];
        let read = stream.read(&mut response).await?;
        Ok::<_, std::io::Error>(response[..read].starts_with(b"HTTP/1.1 200"))
    })
    .await;

    match result {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err("health endpoint did not return 200".into()),
        Ok(Err(e)) => Err(Box::new(e) as BoxError),
        Err(_) => Err("health check timed out".into()),
    }
}
