//! Fixtures shared by the test modules of this binary.
//!
//! Built from `config/stacking-server.example.yaml`, which the config crate asserts is a
//! byte-for-byte copy of PRD §8.2 — so these tests run against the operator's real schema.

use std::net::IpAddr;
use std::sync::Arc;

use astroctl_core::bus::EventBus;
use astroctl_core::config::StackConfig;

use crate::api::{AppState, LoggingInfo, RuntimeSizing, StatusCell};
use crate::auth::AuthPolicy;
use crate::route_meta::RouteDecl;
use crate::vitals::Uptime;

const EXAMPLE: &str = include_str!("../../../config/stacking-server.example.yaml");

/// A stacking server's configuration plus the token it was started with.
pub struct TestNode {
    yaml: String,
    token: Option<String>,
}

impl TestNode {
    /// A node bound to `0.0.0.0` with a token — the production posture.
    pub fn authenticated(token: &str) -> Self {
        Self {
            yaml: EXAMPLE.to_owned(),
            token: Some(token.to_owned()),
        }
    }

    /// A node with no token, bound to loopback — the SDD §4.5 exception.
    pub fn open_loopback() -> Self {
        Self {
            yaml: EXAMPLE.replace("host: 0.0.0.0", "host: 127.0.0.1"),
            token: None,
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

/// Build application state for a node, with the route table the router just declared.
pub fn state_with(node: &TestNode, routes: Vec<RouteDecl>) -> AppState {
    let config = node.config();
    let auth = AuthPolicy::resolve("ASTROCTL_TOKEN", node.token.as_deref(), node.bind())
        .expect("the fixture posture must be startable");
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);

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
    }
}
