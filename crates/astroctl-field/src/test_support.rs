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
            yaml: simulated_devices(EXAMPLE),
            token: Some(token.to_owned()),
        }
    }

    /// A node with no token, bound to loopback — the SDD §4.5 exception.
    pub fn open_loopback() -> Self {
        Self {
            yaml: simulated_devices(&EXAMPLE.replace("host: 0.0.0.0", "host: 127.0.0.1")),
            token: None,
        }
    }

    /// Drive the disk thresholds above the free space of any real volume, so the REL-12 critical
    /// branch fires without needing a full disk.
    ///
    /// The alternative — a loopback filesystem sized to fill — needs root and a mount, which is
    /// not something a unit test may do. Moving the *threshold* exercises the same comparison from
    /// the same `statvfs` reading, which is the part under test.
    pub fn with_disk_critical_above_any_volume(mut self) -> Self {
        self.yaml = self
            .yaml
            .replace("disk_warn_free_gb: 20", "disk_warn_free_gb: 99000")
            .replace("disk_critical_free_gb: 5", "disk_critical_free_gb: 98000");
        self
    }

    /// Pin `camera.default_shutter`.
    pub fn with_shutter(mut self, shutter: &str) -> Self {
        self.yaml = self.yaml.replace(
            "default_shutter: \"30\"",
            &format!("default_shutter: \"{shutter}\""),
        );
        self
    }

    /// Pin `camera.default_format`.
    pub fn with_format(mut self, format: &str) -> Self {
        self.yaml = self.yaml.replace(
            "default_format: \"RAW+JPEG\"",
            &format!("default_format: \"{format}\""),
        );
        self
    }

    /// Pin `camera.timeouts.download_seconds`, so a slow simulated download breaches it.
    ///
    /// SDD §5.3.1's wedged-camera path is only reachable by making the download longer than its
    /// operation-class budget, and the budget is config while the download is a profile knob — so
    /// a test of that path has to move both.
    pub fn with_download_timeout(mut self, seconds: u64) -> Self {
        self.yaml = self.yaml.replace(
            "download_seconds: 120",
            &format!("download_seconds: {seconds}"),
        );
        self
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

/// Point `mount.driver` and `camera.driver` at the simulators, and the session store at a
/// directory a test may write to.
///
/// The example ships `driver: skywatcher` and `driver: gphoto2`, which are right for the operator
/// and wrong for a test suite that has neither a telescope nor a camera: the registry would refuse
/// to build them and every route test would fail at startup rather than at what it was testing.
/// Rewriting the keys rather than special-casing the drivers in [`state_with`] means the tests
/// still go through the real registry lookup, so a driver name that stopped resolving would be
/// caught here.
///
/// `storage.sessions_dir` moves for a blunter reason: `/data/astro/sessions` is the operator's
/// path and does not exist on a build machine, and `FrameStore::open` creates what it is given —
/// so leaving it would have the test suite try to create a directory at the filesystem root.
fn simulated_devices(yaml: &str) -> String {
    let simulated = yaml
        .replace("driver: skywatcher", "driver: simulator")
        .replace("driver: gphoto2", "driver: simulator");
    assert_ne!(
        simulated, yaml,
        "config/field-node.example.yaml no longer selects the skywatcher mount and gphoto2 camera \
         drivers, so the test fixtures are silently running against whatever it does select"
    );

    let sessions = simulated.replace(
        "sessions_dir: /data/astro/sessions",
        &format!("sessions_dir: {}", sessions_dir().display()),
    );
    assert_ne!(
        sessions, simulated,
        "config/field-node.example.yaml no longer puts sessions at /data/astro/sessions, so the \
         test suite is about to write frames wherever it now points"
    );
    sessions
}

/// A session root nothing else is using.
///
/// Under the OS temporary directory rather than `target/`, because `CARGO_TARGET_TMPDIR` is only
/// set for integration tests and these are unit tests. Not deleted on drop: the fixture that owns
/// the path is routinely dropped while the `AppState` built from it is still serving requests
/// (see `mount.rs`'s `node()`), so a `Drop` guard here would delete the session directory out from
/// under a running test. The frames are 128×96 (see [`state_with`]), so what accumulates is
/// kilobytes in a directory the OS already sweeps.
fn sessions_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    std::env::temp_dir()
        .join("astroctl-field-tests")
        .join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
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
///
/// Async since M1-T08 because opening the frame store is: `FrameStore::open` creates the session
/// root and sweeps leftover temporaries, which is I/O and is `tokio::fs`. Doing it here rather
/// than lazily in a route means a test drives the same "session is already open" precondition the
/// binary establishes at startup (SDD §8.1).
pub async fn state_with(node: &TestNode, routes: Vec<RouteDecl>) -> AppState {
    state_with_camera(
        node,
        routes,
        astroctl_drivers::simulator::CameraProfile::fast(),
    )
    .await
}

/// [`state_with`], for a test whose subject is the camera's timing.
///
/// `CameraProfile::fast()` is the right default — 128×96 and no latency, because the measured R10
/// profile is a 48 MB frame and a 2 s download and a test suite is not the place to find out how
/// long that takes forty times over — but a test of SDD §5.3.1's operation-class timeout needs a
/// download that actually takes time.
pub async fn state_with_camera(
    node: &TestNode,
    routes: Vec<RouteDecl>,
    profile: astroctl_drivers::simulator::CameraProfile,
) -> AppState {
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
    // `mount.driver` actually selects rather than one the harness picked — and, since M1-T05,
    // through the same `SafeMount` wrap, so no test can accidentally exercise an unguarded mount
    // and no route can pass a test it would fail in production (ADR-11).
    let bus = EventBus::new();
    let device =
        crate::build_mount(&config, bus.clone()).expect("the fixture must build a mount driver");
    let mount = Arc::new(crate::mount::MountFacade::new(
        device,
        bus.clone(),
        &config.mount,
    ));

    // Through `open_session` and `build_camera`, for the reason the mount goes through
    // `build_mount`: a test that assembled its own approximation would keep passing after the
    // binary's startup sequence changed.
    let store = Arc::new(
        crate::open_session(&config)
            .await
            .expect("the fixture must open a session"),
    );
    let camera = Arc::new(crate::camera::CameraFacade::new(
        crate::build_camera(&config, profile).expect("the fixture must build a camera driver"),
        Arc::clone(&store),
        bus.clone(),
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
        camera,
        tickets: Arc::new(crate::ticket::TicketStore::new()),
        snapshots: Arc::new(crate::ws::SnapshotStore::new()),
        // Idle, exactly as the binary's is until something starts the camera stream. A fixture
        // that pre-started it would make every test that touches `/ws/liveview` depend on a
        // sensor loop none of them asked for.
        liveview: Arc::new(crate::liveview::LiveViewHub::new()),
    }
}
