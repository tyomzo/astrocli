//! Fixtures shared by the test modules of this binary.
//!
//! Built from `config/stacking-server.example.yaml`, which the config crate asserts is a
//! byte-for-byte copy of PRD §8.2 — so these tests run against the operator's real schema, with
//! only the paths and the REL-12 thresholds redirected at a temporary directory.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use astroctl_core::bus::EventBus;
use astroctl_core::config::StackConfig;

use crate::api::{AppState, LoggingInfo, RuntimeSizing, StatusCell};
use crate::auth::AuthPolicy;
use crate::ingest::Ingest;
use crate::mirror::SessionMirror;
use crate::route_meta::RouteDecl;
use crate::vitals::Uptime;

const EXAMPLE: &str = include_str!("../../../config/stacking-server.example.yaml");

/// The archive path in the example configuration, replaced per test.
const EXAMPLE_SESSIONS_DIR: &str = "sessions_dir: /data/astro/sessions";

/// A directory that deletes itself when the test that made it ends.
///
/// Hand-rolled rather than `tempfile`: a dev-dependency has to be declared in the root manifest,
/// which is the one file every parallel task touches, and this is twenty lines.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Create a fresh directory under the system temporary directory.
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astroctl-stack-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // check-async: allow test-only fixture — this file is compiled under `#[cfg(test)]` from
        // main.rs, which the scanner's in-file marker heuristic cannot see.
        std::fs::create_dir_all(&path).expect("the system temporary directory is writable");
        Self(path)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // check-async: allow `Drop` cannot await, and this is test-only cleanup.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A stacking server's configuration, the token it was started with, and its storage.
pub struct TestNode {
    yaml: String,
    token: Option<String>,
    /// Held only for its `Drop`: the archive lives under here, and it goes when the test does.
    _dir: TempDir,
}

impl TestNode {
    /// A node bound to `0.0.0.0` with a token — the production posture.
    pub fn authenticated(token: &str) -> Self {
        Self::new(EXAMPLE.to_owned(), Some(token.to_owned()))
    }

    /// A node with no token, bound to loopback — the SDD §4.5 exception.
    pub fn open_loopback() -> Self {
        Self::new(EXAMPLE.replace("host: 0.0.0.0", "host: 127.0.0.1"), None)
    }

    fn new(yaml: String, token: Option<String>) -> Self {
        let dir = TempDir::new();
        let yaml = yaml.replace(
            EXAMPLE_SESSIONS_DIR,
            &format!("sessions_dir: {}", dir.path().join("sessions").display()),
        );
        Self {
            yaml,
            token,
            _dir: dir,
        }
    }

    /// Pin `server.runtime_worker_threads`.
    pub fn with_worker_threads(mut self, threads: Option<usize>) -> Self {
        let value = threads.map_or_else(|| "null".to_owned(), |n| n.to_string());
        self.yaml = self.yaml.replace(
            "runtime_worker_threads: null",
            &format!("runtime_worker_threads: {value}"),
        );
        self
    }

    /// Pin the REL-12 thresholds.
    ///
    /// Used to drive ingest's 507 path: thresholds above any plausible free space make every
    /// request a refusal, which is the only way to exercise a full disk without filling one.
    pub fn with_disk_thresholds(mut self, warn_gb: f64, critical_gb: f64) -> Self {
        self.yaml = self
            .yaml
            .replace(
                "disk_warn_free_gb: 100",
                &format!("disk_warn_free_gb: {warn_gb}"),
            )
            .replace(
                "disk_critical_free_gb: 20",
                &format!("disk_critical_free_gb: {critical_gb}"),
            );
        self
    }

    /// Parse and validate, exactly as `main` does.
    pub fn config(&self) -> Arc<StackConfig> {
        StackConfig::from_yaml(&self.yaml, "stacking-server.example.yaml")
            .expect("the PRD §8.2 example must always load")
    }

    fn bind(&self) -> IpAddr {
        self.config()
            .server
            .host
            .parse()
            .expect("the config validator guarantees an IP literal")
    }
}

/// A built API surface plus the state behind it, so a test can both call the node and look at
/// what the call did to it.
pub struct TestApp {
    /// The authenticated router. Clone it per request — `oneshot` consumes the service.
    pub router: axum::Router,
    /// The very state the router was built with, for asserting on the bus and the archive.
    pub state: AppState,
}

/// Assemble the node exactly as `main` does: declared routes, real state, bearer layer.
pub async fn app_for(node: &TestNode) -> TestApp {
    let (router, declarations) = crate::api::router();
    let state = state_with(node, declarations).await;
    let auth = Arc::clone(&state.auth);
    TestApp {
        router: crate::api::with_auth(router.with_state(state.clone()), auth),
        state,
    }
}

/// Build a `multipart/form-data` body from named parts, in the order given.
///
/// Hand-rolled so a test can send the parts in the wrong order, omit one, or misname one — the
/// cases a client library would refuse to construct and the handler still has to answer.
///
/// Returns the `Content-Type` header value and the body.
pub fn multipart(parts: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "astroctlTestBoundary8471";
    let mut body = Vec::new();
    for (name, content) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// Lowercase hex SHA-256, as the field node computes it for `frame.saved` (SDD §4.3).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build application state for a node, with the route table the router just declared.
pub async fn state_with(node: &TestNode, routes: Vec<RouteDecl>) -> AppState {
    let config = node.config();
    let auth = AuthPolicy::resolve("ASTROCTL_TOKEN", node.token.as_deref(), node.bind())
        .expect("the fixture posture must be startable");
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let mirror = SessionMirror::open(&config.storage.sessions_dir)
        .await
        .expect("the archive opens under a temporary directory");

    AppState {
        runtime: RuntimeSizing {
            worker_threads: config.server.resolved_worker_threads(cores),
            configured: config.server.runtime_worker_threads,
            available_cores: cores,
        },
        config,
        bus: EventBus::new(),
        status: Arc::new(StatusCell::starting()),
        uptime: Uptime::started_now(),
        auth: Arc::new(auth),
        routes: routes.into(),
        logging: LoggingInfo {
            dir: None,
            error: None,
        },
        ingest: Arc::new(Ingest::new(mirror)),
    }
}
