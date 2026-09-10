//! Shared helpers for the explorer-indexer integration tests.
//!
//! - `pg_fixture()` boots a Postgres testcontainer and returns a
//!   ready-to-use `Db` (with migrations applied). On a developer machine
//!   without Docker it returns `None` and the caller returns early. **In CI it
//!   panics instead** (issue #1283): a silent skip there is a false green, and
//!   the executed-count guard cannot see the difference because a test that
//!   returns early still reports as passed. See
//!   `skills/_shared/test-coverage-policy.md` (loud-skip, never silent-skip).
//! - `StubRpcServer` is an in-process tokio TCP listener that returns
//!   canned JSON-RPC responses keyed by method name, plus a forced-failure
//!   knob. Used to exercise the failure path without a real chain.

#![allow(dead_code)]

use explorer_indexer::Db;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct PgFixture {
    pub db: Db,
    /// Connection URL of the testcontainer, so a test can open its own pool for
    /// raw SQL the `Db` API deliberately does not expose (catalog queries, DDL
    /// for the synthetic block-scoped table in the reorg coverage tests).
    pub url: String,
    // Container handle is held to keep the postgres process alive
    // for the test's lifetime.
    _container: ContainerAsync<Postgres>,
}

fn pg_panic(reason: &str) -> ! {
    panic!(
        "[explorer-indexer-tests] Postgres testcontainer is REQUIRED here but \
         unavailable: {reason}. This test asserts on real schema and real \
         rollback SQL; skipping it would be a silent false green. Give the \
         runner Docker."
    );
}

/// An **unmigrated** Postgres testcontainer: the database is empty, not even
/// `_sqlx_migrations` exists.
///
/// [`pg_fixture`] is this plus `Db::migrate()`. The raw form exists for the
/// migrate-only CLI tests (issue #1359), which must observe the migrator's own
/// effect on a virgin database — running them against an already-migrated
/// fixture would assert nothing, since the migrator would find every version
/// applied and no-op.
pub struct RawPg {
    /// Connection URL of the unmigrated database.
    pub url: String,
    /// Held by the caller to keep the postgres process alive.
    pub container: ContainerAsync<Postgres>,
}

/// Boots Postgres and returns it **without applying migrations**.
pub async fn raw_pg() -> RawPg {
    if which::which("docker").is_err() {
        pg_panic("docker not on PATH");
    }
    let container = match Postgres::default().start().await {
        Ok(c) => c,
        Err(e) => pg_panic(&format!("postgres container failed to start: {e}")),
    };
    let host = match container.get_host().await {
        Ok(h) => h,
        Err(e) => pg_panic(&format!("container host unavailable: {e}")),
    };
    let port = match container.get_host_port_ipv4(5432).await {
        Ok(p) => p,
        Err(e) => pg_panic(&format!("container port unavailable: {e}")),
    };
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    RawPg { url, container }
}

/// Returns `PgFixture` if Docker is available and Postgres came
/// up; panics otherwise.
///
/// Migrations are applied here, via `Db::migrate()` directly — the fixture has
/// never gone through the `indexer` binary's boot path, so removing the boot
/// path's automatic migrate (issue #1359) leaves every fixture-based test
/// unaffected.
pub async fn pg_fixture() -> PgFixture {
    let RawPg { url, container } = raw_pg().await;
    let db = match Db::connect(&url).await {
        Ok(d) => d,
        Err(e) => pg_panic(&format!("db connect failed: {e}")),
    };
    if let Err(e) = db.migrate().await {
        pg_panic(&format!("migrate failed: {e}"));
    }
    PgFixture {
        db,
        url,
        _container: container,
    }
}

/// Tiny in-process JSON-RPC server. Methods can be programmed with
/// canned response strings (already-encoded as JSON values), or set
/// to fail with `force_failure(true)` to simulate an RPC outage.
pub struct StubRpcServer {
    pub url: String,
    handlers: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    fail: Arc<AtomicBool>,
    /// Every JSON-RPC request body received, in arrival order. Lets tests assert
    /// what the indexer actually requested (e.g. the `eth_getLogs` address set).
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl StubRpcServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handlers: Arc<Mutex<HashMap<String, serde_json::Value>>> = Arc::default();
        let fail = Arc::new(AtomicBool::new(false));
        let requests: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let h2 = handlers.clone();
        let f2 = fail.clone();
        let r2 = requests.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let Ok((mut sock, _)) = accept else { break; };
                        let h = h2.clone();
                        let f = f2.clone();
                        let r = r2.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 16 * 1024];
                            let n = match sock.read(&mut buf).await { Ok(n) => n, Err(_) => return };
                            if n == 0 { return; }
                            let body_start = buf[..n].windows(4)
                                .position(|w| w == b"\r\n\r\n")
                                .map(|i| i + 4)
                                .unwrap_or(0);
                            let body = &buf[body_start..n];
                            let req: serde_json::Value = match serde_json::from_slice(body) {
                                Ok(v) => v,
                                Err(_) => return,
                            };
                            r.lock().unwrap().push(req.clone());
                            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                            let resp = if f.load(Ordering::SeqCst) {
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": req.get("id").cloned().unwrap_or(serde_json::json!(1)),
                                    "error": { "code": -32000, "message": "stub forced failure" }
                                })
                            } else {
                                let result = h.lock().unwrap().get(method).cloned().unwrap_or(serde_json::Value::Null);
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": req.get("id").cloned().unwrap_or(serde_json::json!(1)),
                                    "result": result,
                                })
                            };
                            let body = serde_json::to_vec(&resp).unwrap();
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = sock.write_all(header.as_bytes()).await;
                            let _ = sock.write_all(&body).await;
                            let _ = sock.shutdown().await;
                        });
                    }
                }
            }
        });

        Self {
            url,
            handlers,
            fail,
            requests,
            shutdown: shutdown_tx,
        }
    }

    /// All JSON-RPC requests received so far, in arrival order.
    pub fn captured_requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }

    pub fn set(&self, method: &str, value: serde_json::Value) {
        self.handlers
            .lock()
            .unwrap()
            .insert(method.to_string(), value);
    }

    pub fn force_failure(&self, on: bool) {
        self.fail.store(on, Ordering::SeqCst);
    }

    pub fn shutdown(self) {
        let _ = self.shutdown.send(());
    }
}
