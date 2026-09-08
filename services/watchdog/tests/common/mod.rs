//! Shared helpers for watchdog integration tests.
//!
//! - `pg_fixture()` — boots Postgres testcontainer + applies explorer-indexer
//!   migrations (same canonical schema the watchdog queries). **Panics** naming the
//!   missing dependency if Docker/Postgres is unavailable — it never skips.
//! - `MockWebhookServer` — in-process HTTP server for alert payload capture.
//!
//! # Why this fixture panics instead of returning `Option` (issue #1377)
//!
//! It used to be `try_pg_fixture() -> Option<PgFixture>`, and every one of its ten
//! callers wrote `let Some(fx) = try_pg_fixture().await else { return; };`. On a
//! runner without Docker that early `return` is indistinguishable from success: the
//! harness printed `test result: ok. 10 passed` having executed no assertion at all.
//! That is the "loud-skip, never silent-skip" invariant inverted — the suite was
//! greenest exactly when it verified least.
//!
//! Returning `PgFixture` rather than `Option<PgFixture>` closes the shape
//! structurally, not just at today's call sites: a future caller *cannot* write the
//! `else { return; }` guard, because there is no `None` to match.

#![allow(dead_code)]

use explorer_indexer::db::MIGRATOR;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Fixture wrapping a live Postgres container.
pub struct PgFixture {
    pub pool: PgPool,
    _container: ContainerAsync<Postgres>,
}

/// Prefix every unmet-dependency panic carries, so one grep over a job log finds
/// the cause without reading the backtrace.
const MISSING: &str = "[watchdog-tests] REQUIRED DEPENDENCY UNAVAILABLE";

/// Boot a fresh Postgres container, apply the explorer-indexer canonical migrations,
/// and return the pool.
///
/// # Panics
///
/// Panics — naming the missing dependency — when Docker is not on `PATH`, when the
/// Postgres testcontainer cannot be started or addressed, when the pool cannot
/// connect, or when the migrations fail. These integration tests have no meaningful
/// no-Postgres mode: an unmet dependency must red the job, never pass it (#1377).
pub async fn pg_fixture() -> PgFixture {
    assert!(
        which::which("docker").is_ok(),
        "{MISSING}: `docker` is not on PATH. The watchdog integration tests need a \
         Postgres testcontainer (Docker) — install/start Docker, or run only \
         `cargo test -p watchdog --lib`, which needs none. This is a hard failure \
         rather than a skip on purpose: an absent dependency must red the job."
    );
    let container = match Postgres::default().start().await {
        Ok(c) => c,
        Err(e) => panic!(
            "{MISSING}: the Postgres testcontainer failed to start: {e}. Docker is on \
             PATH but not usable (daemon down, socket permissions, or no image pull)."
        ),
    };
    let host = container
        .get_host()
        .await
        .unwrap_or_else(|e| panic!("{MISSING}: Postgres testcontainer host unknown: {e}"));
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .unwrap_or_else(|e| panic!("{MISSING}: Postgres testcontainer port 5432 unmapped: {e}"));
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await
        .unwrap_or_else(|e| {
            panic!("{MISSING}: could not connect to the Postgres testcontainer at {url}: {e}")
        });

    // Apply the canonical explorer-indexer migrations so the watchdog's SQL
    // queries have the expected tables.
    if let Err(e) = MIGRATOR.run(&pool).await {
        panic!("[watchdog-tests] migrate failed: {e}");
    }

    PgFixture {
        pool,
        _container: container,
    }
}

/// Captured webhook request.
#[derive(Debug, Clone)]
pub struct WebhookCapture {
    /// Raw JSON body bytes.
    pub body: Vec<u8>,
}

/// Minimal in-process HTTP server that captures POST /v2/enqueue bodies.
///
/// Responds HTTP 202 to every request so the alert dispatcher sees success.
pub struct MockWebhookServer {
    /// Full URL (e.g. `http://127.0.0.1:12345`).
    pub url: String,
    captures: Arc<Mutex<Vec<WebhookCapture>>>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockWebhookServer {
    /// Start the mock server on a random port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let captures: Arc<Mutex<Vec<WebhookCapture>>> = Arc::default();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let caps2 = captures.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let Ok((mut sock, _)) = accept else { break };
                        let caps = caps2.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 32 * 1024];
                            let n = match sock.read(&mut buf).await { Ok(n) => n, Err(_) => return };
                            if n == 0 { return; }
                            // Split on \r\n\r\n to isolate the JSON body.
                            let body_start = buf[..n].windows(4)
                                .position(|w| w == b"\r\n\r\n")
                                .map(|i| i + 4)
                                .unwrap_or(0);
                            let body = buf[body_start..n].to_vec();
                            caps.lock().unwrap().push(WebhookCapture { body });
                            // 202 Accepted
                            let resp = b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            let _ = sock.write_all(resp).await;
                            let _ = sock.shutdown().await;
                        });
                    }
                }
            }
        });

        Self {
            url,
            captures,
            shutdown: shutdown_tx,
        }
    }

    /// Drain all captured requests so far.
    pub fn drain_captures(&self) -> Vec<WebhookCapture> {
        self.captures.lock().unwrap().drain(..).collect()
    }

    /// Shut down the server.
    pub fn shutdown(self) {
        let _ = self.shutdown.send(());
    }
}

/// A TCP server that accepts a connection and then never responds, holding the
/// socket open until shutdown. Used to simulate a hung RPC endpoint so the
/// watchdog's SLA timeout can be exercised: a pause RPC against this URL never
/// completes and must be aborted by the per-action `timeout`.
pub struct HangingServer {
    /// Full URL (e.g. `http://127.0.0.1:12345`).
    pub url: String,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl HangingServer {
    /// Start the hanging server on a random port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            // Hold accepted sockets open in a vec so they are not dropped (which
            // would close them and let the client error fast). We want the
            // request to hang until the SLA timeout fires.
            let mut held = Vec::new();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        match accept {
                            Ok((sock, _)) => held.push(sock),
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Self {
            url,
            shutdown: shutdown_tx,
        }
    }

    /// Shut down the server.
    pub fn shutdown(self) {
        let _ = self.shutdown.send(());
    }
}
