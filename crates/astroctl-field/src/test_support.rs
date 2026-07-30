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
            yaml: simulated_mount(EXAMPLE),
            token: Some(token.to_owned()),
        }
    }

    /// A node with no token, bound to loopback — the SDD §4.5 exception.
    pub fn open_loopback() -> Self {
        Self {
            yaml: simulated_mount(&EXAMPLE.replace("host: 0.0.0.0", "host: 127.0.0.1")),
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

    /// Mark this node as a non-production deployment.
    pub fn with_deployment_label(mut self, label: &str) -> Self {
        self.yaml = self.yaml.replace(
            "deployment_label: null",
            &format!("deployment_label: {label}"),
        );
        self
    }

    /// Turn on `server.tls`, pointing at a fixture under `testdata/`.
    ///
    /// The example ships the block commented out (absence is what makes plain HTTP the default),
    /// so this appends rather than substitutes.
    pub fn with_tls(mut self, cert: &str, key: &str, warn_days: u32) -> Self {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        self.yaml = append_under_server(
            &self.yaml,
            &format!(
                "  tls:\n    cert_path: {}\n    key_path: {}\n    warn_days_before_expiry: \
                 {warn_days}\n",
                dir.join(cert).display(),
                dir.join(key).display()
            ),
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

/// Point `mount.driver` at the simulator.
///
/// The example ships `driver: skywatcher`, which is right for the operator and wrong for a test
/// suite that has no telescope: the registry would refuse to build it and every route test would
/// fail at startup rather than at what it was testing. Rewriting the key rather than
/// special-casing the driver in [`state_with`] means the tests still go through the real
/// registry lookup, so a driver name that stopped resolving would be caught here.
fn simulated_mount(yaml: &str) -> String {
    let simulated = yaml.replace("driver: skywatcher", "driver: simulator");
    assert_ne!(
        simulated, yaml,
        "config/field-node.example.yaml no longer selects the skywatcher mount driver, so the \
         test fixtures are silently running against whatever it does select"
    );
    simulated
}

/// Append a block to the end of the example, which lands it inside `server`.
///
/// Asserted rather than assumed: a future PRD §8.1 edit that adds a section after `server` would
/// otherwise reparent these keys silently, and `deny_unknown_fields` would report the failure
/// against whatever that new section is — which is a long way from the cause.
fn append_under_server(yaml: &str, block: &str) -> String {
    let last_section = yaml
        .lines()
        .rfind(|line| !line.starts_with([' ', '\t', '#']) && line.contains(':'))
        .expect("the example has top-level sections");
    assert_eq!(
        last_section, "server:",
        "`server` is no longer the last section of config/field-node.example.yaml, so appending \
         no longer lands inside it"
    );
    format!("{yaml}{block}")
}

/// Build application state for a node, with the route table the router just declared.
pub fn state_with(node: &TestNode, routes: Vec<RouteDecl>) -> AppState {
    let config = node.config();
    let auth = AuthPolicy::resolve("ASTROCTL_TOKEN", node.token.as_deref(), node.bind())
        .expect("the fixture posture must be startable");
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);

    // Loaded through the real `tls::load`, so a test that asserts on SEC-07's health fields is
    // asserting on a certificate that was actually parsed, not on a hand-built value.
    let certificate = config
        .server
        .tls
        .as_ref()
        .map(|tls| crate::tls::load(tls).expect("the fixture certificate must load"))
        .map(|materials| materials.status());

    // Through the same `build_mount` the binary uses, so a test drives the driver the operator's
    // `mount.driver` actually selects rather than one the harness picked.
    let device = crate::build_mount(&config).expect("the fixture must build a mount driver");
    let bus = EventBus::new();
    let mount = Arc::new(crate::mount::MountFacade::new(
        device,
        bus.clone(),
        &config.mount,
    ));

    AppState {
        proxy: Arc::new(StackProxy::new(&config.stacking_server)),
        runtime: RuntimeSizing {
            worker_threads: config.server.resolved_worker_threads(cores),
            configured: config.server.runtime_worker_threads,
            available_cores: cores,
        },
        certificate,
        config,
        bus,
        status: Arc::new(StatusCell::starting()),
        uptime: Uptime::started_now(),
        auth: Arc::new(auth),
        routes: routes.into(),
        logging: LoggingInfo {
            dir: None,
            error: None,
        },
        mount,
        tickets: Arc::new(crate::ticket::TicketStore::new()),
        snapshots: Arc::new(crate::ws::SnapshotStore::new()),
    }
}
