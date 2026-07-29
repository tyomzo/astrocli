//! Fixtures shared by the test modules of this binary.
//!
//! Everything is built from `config/field-node.example.yaml` — the same file the config crate
//! asserts is a byte-for-byte copy of PRD §8.1. Tests therefore run against the operator's real
//! schema, and a config change that breaks the node breaks these tests too.

use std::net::IpAddr;
use std::sync::Arc;

use astroctl_core::bus::EventBus;
use astroctl_core::config::FieldConfig;

use crate::api::{AppState, LoggingInfo, RuntimeSizing, StatusCell};
use crate::auth::AuthPolicy;
use crate::proxy::StackProxy;
use crate::route_meta::RouteDecl;
use crate::vitals::Uptime;

const EXAMPLE: &str = include_str!("../../../config/field-node.example.yaml");

/// A field node's configuration plus the token it was started with.
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

    /// Point `stacking_server` at a running test server.
    pub fn with_stack_upstream(mut self, host: &str, port: u16) -> Self {
        self.yaml = self
            .yaml
            .replace("host: 192.168.1.100", &format!("host: {host}"))
            .replace("port: 8471", &format!("port: {port}"));
        self
    }

    /// Turn the stacking server off.
    pub fn with_stack_disabled(mut self) -> Self {
        // `replacen`: `stacking_server` is the first `enabled: true` in the file; `llm` has one
        // too and must keep its value.
        self.yaml = self.yaml.replacen("enabled: true", "enabled: false", 1);
        self
    }

    /// Parse and validate, exactly as `main` does.
    pub fn config(&self) -> Arc<FieldConfig> {
        FieldConfig::from_yaml(&self.yaml, "field-node.example.yaml")
            .expect("the PRD §8.1 example must always load")
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
        proxy: Arc::new(StackProxy::new(&config.stacking_server)),
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
