//! Addressing and operating the M0-T08 container pair.
//!
//! The bash half of the harness (`scripts/lib/harness.sh`) already knows how to resolve a compose
//! implementation, find a container and read its address. This module is the same knowledge in
//! Rust, and the duplication is deliberate: shelling out to the bash library from a test would
//! mean every scenario's failure mode included "the shell script changed", and the two halves
//! answer different questions anyway — bash brings the pair *up*, this brings one node *down*
//! in the middle of a session and puts it back.
//!
//! Nothing here starts the pair from nothing. `scripts/e2e.sh` does that once; a scenario attaches
//! to what is running. That split is what keeps a scenario's runtime in seconds instead of the
//! forty it takes to build two images.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime};

/// The published port the harness uses when compose cannot be asked.
///
/// 18470 and not 8470: `deploy/compose.yaml` chose it so a node already running on the developer's
/// workstation keeps its port, and the suite has to make the same choice or it will cheerfully
/// drive the operator's own field node and report on that instead.
const DEFAULT_FIELD_HOST_PORT: u16 = 18470;

/// How long a node gets to answer `status: ok` after being started.
///
/// Both nodes report `starting` before `ok` (SDD §8.1), so this is not a connect timeout — it is
/// how long the field node may take to open its session store, recover its transfer queue and
/// bind. 90 s matches `dev-up.sh --timeout`; a container on a cold page cache is slower than one
/// that has been up for a minute.
pub const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// A held claim on the one container pair.
///
/// Released on drop, including on a panicking test, because `std` unwinds through `Drop`. A test
/// binary that aborts (a double panic, a `SIGKILL`) leaves the directory behind; [`Lock::acquire`]
/// treats one older than [`Lock::STALE_AFTER`] as abandoned rather than deadlocking a CI job on a
/// crash that already happened.
struct Lock {
    path: PathBuf,
}

impl Lock {
    /// A lock older than this belonged to a process that is not coming back.
    ///
    /// Longer than the slowest scenario (T-HOL-1, which shapes the link to 1 Mbit and then waits
    /// for bytes to cross it) by a wide margin, because the cost of guessing low is two scenarios
    /// stepping on each other and a flake that looks like a product bug.
    const STALE_AFTER: Duration = Duration::from_mins(10);

    fn acquire() -> Self {
        // A directory rather than a file: `create_dir` is atomic and fails if it exists, which is
        // the whole primitive. `File::create` is not — it truncates a lock somebody else holds.
        let path = std::env::temp_dir().join("astroctl-e2e.lock");
        let deadline = Instant::now() + Duration::from_mins(15);
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::is_stale(&path) {
                        eprintln!(
                            "e2e: breaking a lock at {} left by a dead run",
                            path.display()
                        );
                        let _ = std::fs::remove_dir(&path);
                        continue;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "another e2e run has held {} for 15 minutes; remove it if that run is gone",
                        path.display()
                    );
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(error) => panic!("cannot take the e2e lock at {}: {error}", path.display()),
            }
        }
    }

    fn is_stale(path: &Path) -> bool {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .and_then(|at| {
                SystemTime::now()
                    .duration_since(at)
                    .map_err(|_| std::io::Error::other("clock went backwards"))
            })
            .is_ok_and(|age| age > Self::STALE_AFTER)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// The container pair, addressable and operable.
pub struct Harness {
    compose: Vec<OsString>,
    root: PathBuf,
    token: String,
    field_url: String,
    _lock: Lock,
}

impl Harness {
    /// Attach to the running pair, waiting for any other scenario to finish with it first.
    ///
    /// # Panics
    ///
    /// When no compose implementation is on `PATH`, when no token can be found, or when the pair
    /// is not running. All three are "the harness was never brought up" and say so, because the
    /// alternative — a connection-refused twenty lines into a scenario — sends the reader looking
    /// for a product bug.
    #[must_use]
    pub fn attach() -> Self {
        let lock = Lock::acquire();
        let root = repo_root();
        let compose = resolve_compose();
        let token = resolve_token(&root);
        let field_url = resolve_field_url(&compose, &root);

        Self {
            compose,
            root,
            token,
            field_url,
            _lock: lock,
        }
    }

    /// `http://localhost:<published>` — the operator's own path to the field node.
    #[must_use]
    pub fn field_url(&self) -> &str {
        &self.field_url
    }

    /// The shared token both nodes were started with (SEC-02, one token not two).
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// A REST client aimed at the field node.
    #[must_use]
    pub fn client(&self) -> crate::Client {
        crate::Client::new(&self.field_url, &self.token)
    }

    /// Run a compose subcommand against `deploy/compose.yaml`.
    ///
    /// # Panics
    ///
    /// When the command cannot be spawned. A non-zero exit is returned rather than panicked on:
    /// `compose stop` on an already-stopped service is a normal thing for a scenario's cleanup to
    /// do, and the caller is the one that knows whether a failure matters.
    #[must_use]
    pub fn compose(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.compose[0]);
        command.args(&self.compose[1..]);
        command.arg("-f").arg(self.root.join("deploy/compose.yaml"));
        command.args(args);
        // The token has to be in the environment of every compose invocation, not just `up`:
        // compose interpolates `${ASTROCTL_TOKEN}` while *parsing* the file, so a bare `stop`
        // warns about an unset variable and, worse, a later `start` would recreate the container
        // with an empty one.
        command.env("ASTROCTL_TOKEN", &self.token);
        command
            .output()
            .unwrap_or_else(|error| panic!("cannot run {:?}: {error}", self.compose))
    }

    /// Stop a service the way a node dies in the field: the container goes away and, because
    /// `deploy/compose.yaml` sets no `restart:` policy, it stays away until something says
    /// otherwise.
    pub fn stop(&self, service: &str) {
        let output = self.compose(&["stop", "-t", "5", service]);
        assert!(
            output.status.success(),
            "cannot stop {service}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Bring a stopped service back with the volume it had.
    pub fn start(&self, service: &str) {
        let output = self.compose(&["start", service]);
        assert!(
            output.status.success(),
            "cannot start {service}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `docker compose kill -s SIGKILL` — a node that dies without a graceful shutdown.
    ///
    /// The distinction from [`stop`](Self::stop) is the one SDD §7 and REL-04 are about: `stop`
    /// sends SIGTERM and the binary gets to flush; `kill` does not, so what survives is only what
    /// was already durable. Scenarios that assert recovery use this one.
    pub fn kill(&self, service: &str) {
        let output = self.compose(&["kill", "-s", "SIGKILL", service]);
        assert!(
            output.status.success(),
            "cannot kill {service}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Recreate the field container with extra environment, and wait for it to answer.
    ///
    /// The only way to get a fault plan into a container: `ASTROCTL_SIM_FAULTS` is read once at
    /// startup by `build_mount` (SDD §9 — fault injection is a constructor parameter, and a
    /// process is where a container's constructors run). Recreating rather than restarting is
    /// required because compose fixes a container's environment when it is created.
    ///
    /// The named volume survives, so the session, the journal and the event log are the ones the
    /// previous incarnation left — which is what makes this also the mechanism for the
    /// restart-recovery scenario, with no extra environment at all.
    ///
    /// # Panics
    ///
    /// When compose refuses, or when the node does not come back.
    pub async fn recreate_field(&self, env: &[(&str, &str)]) {
        let mut command = Command::new(&self.compose[0]);
        command.args(&self.compose[1..]);
        command.arg("-f").arg(self.root.join("deploy/compose.yaml"));
        command.args(["up", "-d", "--force-recreate", "--no-deps", "field"]);
        command.env("ASTROCTL_TOKEN", &self.token);
        // Every variable compose.yaml interpolates has to be present on every invocation, or the
        // recreated container gets an empty one. Absent keys are explicitly cleared rather than
        // left to inherit this process's environment: a scenario that ran after one which set
        // faults must get a clean mount, and inheriting would make that depend on test order.
        command.env("ASTROCTL_SIM_FAULTS", "");
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("cannot recreate the field node: {error}"));
        assert!(
            output.status.success(),
            "cannot recreate the field node: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.wait_field_ready(READY_TIMEOUT).await;
    }

    /// Run a command inside a service container and return its stdout.
    ///
    /// `-T` disables TTY allocation, without which this hangs when stdin is not a terminal — i.e.
    /// under `cargo test`, i.e. always.
    ///
    /// # Panics
    ///
    /// When the command fails inside the container. A scenario uses this to look at the volume,
    /// and a failure there is a failure of the assertion it was making.
    #[must_use]
    pub fn exec(&self, service: &str, argv: &[&str]) -> String {
        let mut compose_args = vec!["exec", "-T", service];
        compose_args.extend_from_slice(argv);
        let output = self.compose(&compose_args);
        assert!(
            output.status.success(),
            "`{}` failed inside {service}: {}",
            argv.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Whether a path exists inside a service container.
    ///
    /// Separate from [`exec`](Self::exec) because "the file is not there" is an answer this suite
    /// asserts on, not a failure to run a command.
    #[must_use]
    pub fn path_exists(&self, service: &str, path: &str) -> bool {
        let output = self.compose(&["exec", "-T", service, "test", "-f", path]);
        output.status.success()
    }

    /// Wait until the field node answers `status: ok` on its own health route.
    ///
    /// # Panics
    ///
    /// On timeout, with the last response body, because "did not become ready" without it is a
    /// message that sends the reader to `docker compose logs` for information the wait already had.
    pub async fn wait_field_ready(&self, timeout: Duration) {
        self.wait_ready("field node", "/api/system/health", timeout)
            .await;
    }

    /// Wait until the stacking server answers **through the field node's proxy**.
    ///
    /// Through `/stack/*` deliberately, as `dev-up.sh` does: the stack node publishes no port, so
    /// this is the only route anything in this system has to it. A stack node that is up but
    /// unreachable through the proxy is not up as far as the operator is concerned.
    pub async fn wait_stack_ready(&self, timeout: Duration) {
        self.wait_ready(
            "stacking server (through /stack/*)",
            "/stack/api/system/health",
            timeout,
        )
        .await;
    }

    async fn wait_ready(&self, label: &str, path: &str, timeout: Duration) {
        let client = self.client();
        let deadline = Instant::now() + timeout;
        loop {
            // The last response is bound inside the loop, so the message a timeout prints is
            // always the most recent answer rather than whatever a declaration outside was
            // initialised to. Both nodes answer `starting` before `ok`, and "it said starting for
            // ninety seconds" and "it refused the connection for ninety seconds" send the reader
            // to different places.
            let last = match client.get_text(path).await {
                Ok(body) => {
                    if body.contains("\"status\":\"ok\"") {
                        return;
                    }
                    body
                }
                Err(error) => error.to_string(),
            };
            assert!(
                Instant::now() < deadline,
                "{label} did not report ok within {timeout:?}; last response: {last}"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Put the pair back the way a scenario found it: both services running and answering.
    ///
    /// Called at the *start* of every scenario rather than at the end of the previous one. The
    /// difference matters when a scenario fails: a cleanup in the failing test never runs past the
    /// panic, so the next scenario would inherit a stopped node and fail for a reason that is not
    /// its own. Restoring on entry means exactly one test reports each fault.
    pub async fn ensure_pair_running(&self) {
        let output = self.compose(&["ps", "--status", "running", "--services"]);
        let running = String::from_utf8_lossy(&output.stdout);
        let running: Vec<&str> = running.split_whitespace().collect();
        for service in ["field", "stack"] {
            if !running.contains(&service) {
                eprintln!("e2e: {service} was not running; starting it");
                self.start(service);
            }
        }
        self.wait_field_ready(READY_TIMEOUT).await;
        self.wait_stack_ready(READY_TIMEOUT).await;

        // A field node still holding a fault plan would fail whichever scenario ran next, for a
        // reason belonging to the one before it. Checking here rather than cleaning up there is
        // the same argument as the paragraph above: the cleanup at the end of a scenario that
        // panicked never runs, and the cost of that must not be paid by the next test.
        let armed = self
            .compose(&[
                "exec",
                "-T",
                "field",
                "sh",
                "-c",
                "printenv ASTROCTL_SIM_FAULTS || true",
            ])
            .stdout;
        if !String::from_utf8_lossy(&armed).trim().is_empty() {
            eprintln!("e2e: the field node still holds a fault plan; recreating it clean");
            self.recreate_field(&[]).await;
        }
    }

    /// The field node's session event log (SDD §2 logging, `server.log_dir/events.jsonl`), as text.
    ///
    /// Read out of the container rather than off a host bind mount because the harness uses a
    /// *named* volume — which is what makes restart recovery testable (REL-06) and what means the
    /// host has no path to the file.
    #[must_use]
    pub fn field_event_log(&self) -> String {
        self.exec("field", &["cat", "/data/astro/logs/events.jsonl"])
    }

    /// Where the repository is, for scenarios that need to reach a script.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run a repository script (`scripts/shape-link.sh`, …) with the harness's token in scope.
    ///
    /// # Panics
    ///
    /// When the script cannot be spawned.
    #[must_use]
    pub fn script(&self, name: &str, args: &[&str]) -> Output {
        Command::new(self.root.join("scripts").join(name))
            .args(args)
            .env("ASTROCTL_TOKEN", &self.token)
            .current_dir(&self.root)
            .output()
            .unwrap_or_else(|error| panic!("cannot run scripts/{name}: {error}"))
    }
}

/// The repository root, derived from this file's compile-time path.
///
/// `CARGO_MANIFEST_DIR` and not the current directory: `cargo test` sets the working directory to
/// the manifest directory, but a scenario run from an IDE or a `cargo test --manifest-path` from
/// the repository root does not, and a harness that resolves `deploy/compose.yaml` differently
/// depending on where it was invoked is a harness with two behaviours.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tests/e2e is two levels below the repository root")
        .to_path_buf()
}

/// Resolve a compose implementation, mirroring `scripts/lib/harness.sh`'s order.
fn resolve_compose() -> Vec<OsString> {
    if let Ok(override_value) = std::env::var("ASTROCTL_COMPOSE") {
        return override_value
            .split_whitespace()
            .map(OsString::from)
            .collect();
    }
    for candidate in [
        vec!["docker", "compose"],
        vec!["podman", "compose"],
        vec!["podman-compose"],
    ] {
        let ok = Command::new(candidate[0])
            .args(&candidate[1..])
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success());
        if ok {
            return candidate.into_iter().map(OsString::from).collect();
        }
    }
    panic!(
        "no compose implementation found (tried `docker compose`, `podman compose`, \
         `podman-compose`). Set ASTROCTL_COMPOSE, or run scripts/e2e.sh which checks this first."
    );
}

/// The shared token, from the environment or from the file `dev-up.sh` wrote.
///
/// Same precedence as `dev-up.sh`: an `ASTROCTL_TOKEN` already exported wins, because that is
/// compose's own precedence and two rules would be one too many.
fn resolve_token(root: &Path) -> String {
    if let Ok(token) = std::env::var("ASTROCTL_TOKEN") {
        if !token.is_empty() {
            return token;
        }
    }
    let env_file = root.join("deploy/.env");
    let contents = std::fs::read_to_string(&env_file).unwrap_or_else(|error| {
        panic!(
            "no ASTROCTL_TOKEN in the environment and cannot read {}: {error}\n\
             Run scripts/e2e.sh, or scripts/dev-up.sh, which generates one.",
            env_file.display()
        )
    });
    contents
        .lines()
        .find_map(|line| line.strip_prefix("ASTROCTL_TOKEN="))
        .map_or_else(
            || panic!("{} has no ASTROCTL_TOKEN= line", env_file.display()),
            |token| token.trim().to_owned(),
        )
}

/// Ask compose which host port it published, falling back the way `dev-up.sh` does.
fn resolve_field_url(compose: &[OsString], root: &Path) -> String {
    let mut command = Command::new(&compose[0]);
    command.args(&compose[1..]);
    command.arg("-f").arg(root.join("deploy/compose.yaml"));
    command.args(["port", "field", "8470"]);
    // Parsing succeeds only when the container is running, so an empty answer is also the
    // "the pair is not up" diagnostic.
    let published = command
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();

    let port = published
        .rsplit(':')
        .next()
        .and_then(|port| port.trim().parse::<u16>().ok())
        .or_else(|| {
            std::env::var("ASTROCTL_FIELD_HOST_PORT")
                .ok()
                .and_then(|port| port.parse().ok())
        })
        .unwrap_or(DEFAULT_FIELD_HOST_PORT);

    format!("http://localhost:{port}")
}
