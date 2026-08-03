//! Typed, validated, fail-loud configuration for both nodes (SDD §4.4).
//!
//! The structs below mirror PRD §8.1 (field node) and §8.2 (stacking server) exactly, with
//! `#[serde(deny_unknown_fields)]` at every level. A typo in the operator's YAML is a startup
//! error naming the offending key, never silent default behaviour. That property cuts both
//! ways: the PRD examples and these structs must agree exactly, in *both* directions, which is
//! why `config/field-node.example.yaml` and `config/stacking-server.example.yaml` are
//! byte-for-byte copies of the PRD blocks and the test module asserts both that they are still
//! copies and that they still load. If the design grows a key the PRD lacks, the PRD is what
//! gets fixed.
//!
//! Parsing is only half the contract. [`load_field_config`] and [`load_stack_config`] run a
//! post-parse pass that expands `~` in paths and then range-checks every numeric key,
//! cross-checks the keys that constrain each other (TTL default vs. max, disk warn vs.
//! critical, driver vs. its address key), and reports *every* problem it finds at once, each
//! one naming its full dotted YAML path.
//!
//! The loaded, validated config is handed out as an `Arc` and there is deliberately no reload
//! or re-read API: SDD §4.4 makes the loaded value the single instance every component shares.
//!
//! Secrets are never in here. `server.auth_token_env` and `llm.api_key_env` hold environment
//! variable *names* ([`EnvVarName`]); the value is fetched on demand as a [`Secret`], whose
//! `Debug` and `Display` are redacted so a token cannot reach a log line by accident (SEC-04).

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Deserialize;

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// One rejected key, named by its full dotted YAML path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError {
    /// Dotted YAML path, e.g. `mount.limits.min_altitude_degrees`.
    pub key: String,
    /// What is wrong and what the operator should write instead.
    pub message: String,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

/// Everything that can go wrong between "operator has a YAML file" and "node has a config".
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read at all.
    #[error("cannot read configuration file `{file}`: {source}")]
    Io {
        /// Path we tried to read.
        file: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },

    /// The YAML did not parse, or did not match the schema (unknown key, missing key, wrong
    /// type). The wrapped error already carries the dotted YAML path and the line/column.
    #[error("invalid configuration in `{file}`: {source}")]
    Parse {
        /// Path we were parsing.
        file: PathBuf,
        /// Parser/serde error, e.g. ``mount.limits: unknown field `slew_ttl_maxx_ms` …``.
        #[source]
        source: yaml_serde::Error,
    },

    /// The YAML parsed, but one or more values are out of range or contradict each other.
    #[error("{}", render_invalid(.file, .errors))]
    Invalid {
        /// Path we validated.
        file: PathBuf,
        /// Every problem found, not just the first.
        errors: Vec<KeyError>,
    },

    /// A `*_env` key names an environment variable that is not set.
    #[error("{key}: environment variable `{name}` is not set")]
    MissingEnv {
        /// Dotted YAML path of the key holding the variable name.
        key: String,
        /// The environment variable name that was not set.
        name: String,
    },
}

fn render_invalid(file: &Path, errors: &[KeyError]) -> String {
    use fmt::Write as _;

    let mut out = format!(
        "{} configuration error{} in `{}`:",
        errors.len(),
        if errors.len() == 1 { "" } else { "s" },
        file.display()
    );
    for e in errors {
        // Writing into a String cannot fail; the result is discarded deliberately.
        let _ = write!(out, "\n  {e}");
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------------------------

/// The *name* of an environment variable holding a credential (SEC-04).
///
/// The name is not secret and is shown by `Debug` — knowing which variable to export is exactly
/// what an operator debugging a startup refusal needs. The value never enters this struct; it
/// is fetched on demand by [`EnvVarName::read`] and arrives wrapped in [`Secret`].
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct EnvVarName(String);

impl EnvVarName {
    /// The environment variable name as written in the YAML.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }

    /// Read the variable, failing with a message that names both the YAML key and the variable.
    ///
    /// # Errors
    /// [`ConfigError::MissingEnv`] if the variable is absent or not valid Unicode.
    pub fn read(&self, key: &str) -> Result<Secret, ConfigError> {
        match std::env::var(&self.0) {
            Ok(v) => Ok(Secret(v)),
            Err(_) => Err(ConfigError::MissingEnv {
                key: key.to_owned(),
                name: self.0.clone(),
            }),
        }
    }

    /// Read the variable if it is set, without treating absence as an error.
    #[must_use]
    pub fn read_optional(&self) -> Option<Secret> {
        std::env::var(&self.0).ok().map(Secret)
    }
}

/// Redacted `Debug`: shows which variable was named, never what is in it.
impl fmt::Debug for EnvVarName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EnvVarName({:?} => <redacted>)", self.0)
    }
}

/// A credential read out of the environment. Neither `Debug` nor `Display` ever reveals it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The secret itself. Every call site is a place a token could leak, so this is the one
    /// deliberately awkward accessor rather than a `Deref`.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

// ---------------------------------------------------------------------------------------------
// Validation harness
// ---------------------------------------------------------------------------------------------

/// Accumulates validation failures with the dotted YAML path of whatever section is being
/// walked, so every message can name its key without each check repeating the prefix.
pub struct Check {
    path: Vec<&'static str>,
    errors: Vec<KeyError>,
    home: Option<PathBuf>,
}

impl Check {
    fn new(home: Option<PathBuf>) -> Self {
        Self {
            path: Vec::new(),
            errors: Vec::new(),
            home,
        }
    }

    fn section(&mut self, name: &'static str, f: impl FnOnce(&mut Self)) {
        self.path.push(name);
        f(self);
        self.path.pop();
    }

    fn key(&self, name: &str) -> String {
        if self.path.is_empty() {
            name.to_owned()
        } else {
            format!("{}.{name}", self.path.join("."))
        }
    }

    fn fail(&mut self, name: &str, message: impl Into<String>) {
        let key = self.key(name);
        self.errors.push(KeyError {
            key,
            message: message.into(),
        });
    }

    /// Integer range check, inclusive on both ends.
    fn range<T>(&mut self, name: &str, value: T, lo: T, hi: T)
    where
        T: PartialOrd + fmt::Display + Copy,
    {
        if !(lo..=hi).contains(&value) {
            self.fail(
                name,
                format!("{value} is out of range; expected {lo}..={hi}"),
            );
        }
    }

    /// Float range check that also rejects NaN and infinities — YAML `.nan` and `.inf` are
    /// real inputs, and NaN silently passes every comparison-based bound.
    fn range_f64(&mut self, name: &str, value: f64, lo: f64, hi: f64) {
        if !value.is_finite() {
            self.fail(name, format!("{value} is not a finite number"));
        } else if !(lo..=hi).contains(&value) {
            self.fail(
                name,
                format!("{value} is out of range; expected {lo}..={hi}"),
            );
        }
    }

    fn non_empty(&mut self, name: &str, value: &str) {
        if value.trim().is_empty() {
            self.fail(name, "must not be empty");
        }
    }

    /// POSIX environment variable name: letters, digits and underscore, not starting with a
    /// digit. A name the shell cannot export is a guaranteed startup failure later.
    fn env_var_name(&mut self, name: &str, value: &EnvVarName) {
        let raw = value.name();
        let valid = !raw.is_empty()
            && !raw.starts_with(|c: char| c.is_ascii_digit())
            && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            self.fail(
                name,
                format!(
                    "`{raw}` is not a valid environment variable name; expected letters, \
                     digits and underscores, not starting with a digit (e.g. ASTROCTL_TOKEN)"
                ),
            );
        }
    }

    fn absolute(&mut self, name: &str, value: &Path) {
        if !value.is_absolute() {
            self.fail(
                name,
                format!(
                    "`{}` is not an absolute path; give the full path (e.g. /data/astro/sessions)",
                    value.display()
                ),
            );
        }
    }

    /// A bind address must be an IP literal — a hostname resolves to an arbitrary set of
    /// interfaces, which is the opposite of what SEC-01 asks for.
    fn bind_address(&mut self, name: &str, value: &str) {
        if value.parse::<IpAddr>().is_err() {
            self.fail(
                name,
                format!(
                    "`{value}` is not an IP address; a bind address must be a literal such as \
                     0.0.0.0, 127.0.0.1 or the node's VPN address (SEC-01)"
                ),
            );
        }
    }

    fn http_url(&mut self, name: &str, value: &str) {
        if !(value.starts_with("http://") || value.starts_with("https://")) {
            self.fail(
                name,
                format!("`{value}` must be a URL starting with http:// or https://"),
            );
        }
    }

    /// A bare host or IP for an outbound connection — hostnames are legitimate here, but a URL
    /// or an embedded port is not, because the port has its own key.
    fn host_name(&mut self, name: &str, value: &str) {
        if value.trim().is_empty() {
            self.fail(name, "must not be empty");
        } else if value.contains("://") || value.contains('/') {
            self.fail(
                name,
                format!("`{value}` must be a bare host name or IP address, not a URL"),
            );
        } else if value.contains(char::is_whitespace) {
            self.fail(name, format!("`{value}` must not contain whitespace"));
        }
    }

    /// Expand a leading `~` and record a named error rather than silently leaving it unexpanded
    /// — a literal `~` directory is one of the more baffling things to find on a Pi at 2am.
    fn expand(&mut self, name: &str, value: &mut PathBuf) {
        match expand_tilde(value, self.home.as_deref()) {
            Ok(expanded) => *value = expanded,
            Err(message) => self.fail(name, message),
        }
    }
}

fn expand_tilde(path: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let Some(text) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if text == "~" || text.starts_with("~/") {
        let Some(home) = home else {
            return Err(format!(
                "cannot expand `~` in `{text}`: the HOME environment variable is not set; \
                 write the full path instead"
            ));
        };
        let rest = text.strip_prefix("~/").unwrap_or("");
        return Ok(if rest.is_empty() {
            home.to_path_buf()
        } else {
            home.join(rest)
        });
    }
    if text.starts_with('~') {
        return Err(format!(
            "`{text}` uses `~user` home expansion, which is not supported; write the full path"
        ));
    }
    Ok(path.to_path_buf())
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

/// Internal contract shared by the two node configs: normalize (path expansion), then validate.
trait Configuration: DeserializeOwned {
    fn normalize(&mut self, c: &mut Check);
    fn validate(&self, c: &mut Check);
}

fn parse_and_validate<T: Configuration>(text: &str, file: &Path) -> Result<Arc<T>, ConfigError> {
    let mut cfg: T = yaml_serde::from_str(text).map_err(|source| ConfigError::Parse {
        file: file.to_path_buf(),
        source,
    })?;

    let mut check = Check::new(std::env::var_os("HOME").map(PathBuf::from));
    cfg.normalize(&mut check);
    cfg.validate(&mut check);

    if check.errors.is_empty() {
        Ok(Arc::new(cfg))
    } else {
        Err(ConfigError::Invalid {
            file: file.to_path_buf(),
            errors: check.errors,
        })
    }
}

fn load<T: Configuration>(file: &Path) -> Result<Arc<T>, ConfigError> {
    let text = fs::read_to_string(file).map_err(|source| ConfigError::Io {
        file: file.to_path_buf(),
        source,
    })?;
    parse_and_validate(&text, file)
}

/// Load and validate a field-node configuration (PRD §8.1).
///
/// # Errors
/// [`ConfigError`] if the file cannot be read, does not match the schema, or holds values that
/// are out of range or mutually inconsistent.
pub fn load_field_config(file: impl AsRef<Path>) -> Result<Arc<FieldConfig>, ConfigError> {
    load(file.as_ref())
}

/// Load and validate a stacking-server configuration (PRD §8.2).
///
/// # Errors
/// As [`load_field_config`].
pub fn load_stack_config(file: impl AsRef<Path>) -> Result<Arc<StackConfig>, ConfigError> {
    load(file.as_ref())
}

impl FieldConfig {
    /// Parse and validate from a YAML string. `origin` is used only to name the source in
    /// errors, so callers without a real file can pass something descriptive.
    ///
    /// # Errors
    /// As [`load_field_config`].
    pub fn from_yaml(text: &str, origin: impl AsRef<Path>) -> Result<Arc<Self>, ConfigError> {
        parse_and_validate(text, origin.as_ref())
    }
}

impl StackConfig {
    /// Parse and validate from a YAML string. `origin` names the source in errors only.
    ///
    /// # Errors
    /// As [`load_stack_config`].
    pub fn from_yaml(text: &str, origin: impl AsRef<Path>) -> Result<Arc<Self>, ConfigError> {
        parse_and_validate(text, origin.as_ref())
    }
}

// ---------------------------------------------------------------------------------------------
// Shared enums and sections
// ---------------------------------------------------------------------------------------------

/// Tracing verbosity. Written uppercase in the PRD examples; lowercase is accepted as an alias
/// because rejecting `info` teaches the operator nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    /// Everything, including per-frame protocol traffic.
    #[serde(alias = "trace")]
    Trace,
    /// Developer-level detail.
    #[serde(alias = "debug")]
    Debug,
    /// Default operating level.
    #[serde(alias = "info")]
    Info,
    /// Degradations that do not stop the session.
    #[serde(alias = "warn")]
    Warn,
    /// Failures.
    #[serde(alias = "error")]
    Error,
}

/// Where sessions, frames and logs live, and the free-space thresholds that gate capture
/// (PRD §8.1/§8.2 `storage`, REL-12). Identical shape on both nodes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Root of the session tree (PRD §5.9 layout).
    pub sessions_dir: PathBuf,
    /// Free space below which a warning alert is raised.
    pub disk_warn_free_gb: f64,
    /// Free space below which capture pauses (field) / ingest rejects (stack).
    pub disk_critical_free_gb: f64,
}

impl StorageConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("sessions_dir", &mut self.sessions_dir);
    }

    fn validate(&self, c: &mut Check) {
        c.absolute("sessions_dir", &self.sessions_dir);
        c.range_f64("disk_warn_free_gb", self.disk_warn_free_gb, 0.0, 100_000.0);
        c.range_f64(
            "disk_critical_free_gb",
            self.disk_critical_free_gb,
            0.0,
            100_000.0,
        );
        if self.disk_critical_free_gb >= self.disk_warn_free_gb {
            c.fail(
                "disk_critical_free_gb",
                format!(
                    "{} must be below disk_warn_free_gb ({}) — the warning has to arrive \
                     before the stop (REL-12)",
                    self.disk_critical_free_gb, self.disk_warn_free_gb
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Field node (PRD §8.1)
// ---------------------------------------------------------------------------------------------

/// The whole field-node configuration, exactly PRD §8.1.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldConfig {
    /// Observing site.
    pub site: SiteConfig,
    /// Mount driver, transport and safety limits.
    pub mount: MountConfig,
    /// Camera driver, defaults and operation-class timeouts.
    pub camera: CameraConfig,
    /// Session/frame/log storage and disk thresholds.
    pub storage: StorageConfig,
    /// Guide camera (Phase 3; `driver: null` disables it).
    pub guide_camera: GuideCameraConfig,
    /// Guiding parameters (Phase 3 keys optional).
    pub guider: GuiderConfig,
    /// Plate-solver backend and search parameters.
    pub solver: SolverConfig,
    /// Equipment profile stamped onto frames for calibration matching.
    pub equipment: EquipmentConfig,
    /// Stacking-server address, transfer method and upload pacing.
    pub stacking_server: StackingServerConfig,
    /// LLM control layer (Phase 2c; parsed and validated from Phase 1).
    pub llm: LlmConfig,
    /// HTTP server, auth, staleness window and runtime sizing.
    pub server: FieldServerConfig,
}

impl Configuration for FieldConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.section("storage", |c| self.storage.normalize(c));
        c.section("solver", |c| self.solver.normalize(c));
        c.section("stacking_server", |c| self.stacking_server.normalize(c));
        c.section("server", |c| self.server.normalize(c));
    }

    fn validate(&self, c: &mut Check) {
        c.section("site", |c| self.site.validate(c));
        c.section("mount", |c| self.mount.validate(c));
        c.section("camera", |c| self.camera.validate(c));
        c.section("storage", |c| self.storage.validate(c));
        c.section("guide_camera", |c| self.guide_camera.validate(c));
        c.section("guider", |c| self.guider.validate(c));
        c.section("solver", |c| self.solver.validate(c));
        c.section("equipment", |c| self.equipment.validate(c));
        c.section("stacking_server", |c| self.stacking_server.validate(c));
        c.section("llm", |c| self.llm.validate(c));
        c.section("server", |c| self.server.validate(c));
    }
}

impl FieldConfig {
    /// The shared bearer token for this node (SEC-02), read from the environment on demand.
    ///
    /// # Errors
    /// [`ConfigError::MissingEnv`] if `server.auth_token_env` names an unset variable.
    pub fn auth_token(&self) -> Result<Secret, ConfigError> {
        self.server.auth_token_env.read("server.auth_token_env")
    }
}

/// Observing site (PRD §8.1 `site`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    /// Degrees north, `[-90, 90]`.
    pub latitude: f64,
    /// Degrees east, `[-180, 180]`.
    pub longitude: f64,
    /// Metres above sea level.
    pub elevation: f64,
    /// IANA timezone name, e.g. `Europe/Vilnius`.
    pub timezone: String,
}

impl SiteConfig {
    fn validate(&self, c: &mut Check) {
        c.range_f64("latitude", self.latitude, -90.0, 90.0);
        c.range_f64("longitude", self.longitude, -180.0, 180.0);
        c.range_f64("elevation", self.elevation, -500.0, 9000.0);
        c.non_empty("timezone", &self.timezone);
        if !self.timezone.trim().is_empty()
            && self.timezone != "UTC"
            && !self.timezone.contains('/')
        {
            c.fail(
                "timezone",
                format!(
                    "`{}` is not an IANA timezone name; expected `UTC` or `Area/Location` \
                     (e.g. Europe/Vilnius)",
                    self.timezone
                ),
            );
        }
    }
}

/// Mount driver selection (PRD §8.1 `mount.driver`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountDriver {
    /// Sky-Watcher motor-controller protocol (PRD §4.2).
    Skywatcher,
    /// Any INDI-served mount, via `indi_device`.
    Indi,
    /// ASCOM Alpaca, via `ascom_host`.
    AscomAlpaca,
    /// In-process simulator (HAL-11).
    Simulator,
}

impl MountDriver {
    /// The name this selection resolves to in the HAL driver registry (HAL-07, SDD §5.1).
    ///
    /// The registry is keyed by these strings — a driver whose factory reports a different
    /// spelling is simply unreachable from configuration, and the failure is a startup error
    /// naming a driver the operator can see in their own YAML. The test below pins each string
    /// to what serde accepts, so the two spellings cannot drift apart.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skywatcher => "skywatcher",
            Self::Indi => "indi",
            Self::AscomAlpaca => "ascom_alpaca",
            Self::Simulator => "simulator",
        }
    }
}

/// Mount configuration (PRD §8.1 `mount`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    /// Which driver implementation to build.
    pub driver: MountDriver,
    /// `auto` for USB VID/PID detection, or a device node such as `/dev/ttyUSB0`.
    pub port: String,
    /// Serial line rate; 9600 for the HEQ5 Pro (PRD §4.2).
    pub baud: u32,
    /// Pause after a slew before capture starts.
    pub settle_time_seconds: u32,
    /// Serial request timing and watchdog thresholds (SDD §5.2.4).
    pub serial: SerialConfig,
    /// Safety limits and the manual-slew dead-man's switch (SDD §5.4, §5.8.1).
    pub limits: MountLimits,
    /// Rig and tripod dimensions for the collision check (SDD §5.4.3, Layer 2).
    ///
    /// **Absent by default, and that is the honest default.** Every number here is a measurement
    /// of one physical installation; a shipped guess would be a limit enforced against a quantity
    /// nobody measured, which is the objection that deleted `park_position` in M3-T07. A node with
    /// no geometry has no collision limit and says so, rather than having one that is wrong.
    #[serde(default)]
    pub geometry: Option<RigGeometry>,
    /// INDI device name; required when `driver: indi`.
    #[serde(default)]
    pub indi_device: Option<String>,
    /// Alpaca base URL; required when `driver: ascom_alpaca`.
    #[serde(default)]
    pub ascom_host: Option<String>,
}

/// Standard serial line rates. A non-standard rate is almost always a typo, and the failure it
/// produces otherwise is a silent stream of garbled frames rather than an error.
const STANDARD_BAUD: &[u32] = &[
    1200, 2400, 4800, 9600, 19200, 38400, 57600, 115_200, 230_400, 460_800, 921_600,
];

impl MountConfig {
    fn validate(&self, c: &mut Check) {
        if self.port != "auto" && !Path::new(&self.port).is_absolute() {
            c.fail(
                "port",
                format!(
                    "`{}` is neither `auto` nor a device node path such as /dev/ttyUSB0",
                    self.port
                ),
            );
        }
        if !STANDARD_BAUD.contains(&self.baud) {
            let list = STANDARD_BAUD
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            c.fail(
                "baud",
                format!(
                    "{} is not a standard serial rate; expected one of {list}",
                    self.baud
                ),
            );
        }
        c.range("settle_time_seconds", self.settle_time_seconds, 0, 300);
        c.section("serial", |c| self.serial.validate(c));
        c.section("limits", |c| self.limits.validate(c));
        if let Some(geometry) = self.geometry.as_ref() {
            c.section("geometry", |c| geometry.validate(c));
        }

        match self.driver {
            MountDriver::Indi if self.indi_device.is_none() => c.fail(
                "indi_device",
                "required when `mount.driver` is `indi`: name the INDI device, e.g. \"EQMod Mount\"",
            ),
            MountDriver::AscomAlpaca => match &self.ascom_host {
                None => c.fail(
                    "ascom_host",
                    "required when `mount.driver` is `ascom_alpaca`: give the Alpaca base URL",
                ),
                Some(host) => c.http_url("ascom_host", host),
            },
            _ => {}
        }
    }
}

// `mount.park_position` was here until M3-T07, and its removal is the fix rather than a tidy-up.
//
// It was a *sky* coordinate, shipped as `ra_hours: 0.0, dec_degrees: 90.0`. At declination 90 the
// right ascension is degenerate — every value names the celestial pole — so the goto it produced
// was satisfied by the declination axis alone and left the right-ascension axis wherever it
// happened to be. Observed on 2026-08-02 with that axis 215.6° from home while the app reported
// `parked: true`.
//
// No value of this key could have fixed that. Park's contract is not "point at the pole", it is
// "return to the pose power-on will assume" — both counters at `0x800000` — and that pose is a
// mechanical fact of the mount, not an operator preference. A sky coordinate cannot express it
// (any target with `dec = 90` under-constrains one axis by construction, and any target with
// `dec ≠ 90` is not the home pose), and an axis-angle pair could express it but only by letting
// an operator configure a pose at which power-on would then lie about where the mount is. So
// there is nothing left for the key to say, and `deny_unknown_fields` now rejects a configuration
// that still carries it — see `tests::a_configuration_that_still_names_a_park_position_is_refused`.
//
// A separate "stow" pose (tube horizontal for a cover, say) would be a different concept with
// different semantics, and can arrive with its own key when something needs it. Keeping this one
// warm for that hypothetical would ship a key that is read and ignored.

/// Serial request timing and watchdog thresholds (PRD §8.1 `mount.serial`, SDD §5.2.4).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    /// Per request/response exchange timeout. 500 ms is ~30x the measured 16.6 ms worst case.
    pub request_timeout_ms: u64,
    /// Retries before the driver reports `DeviceError::Timeout`.
    pub request_retries: u32,
    /// Consecutive poll failures before the watchdog fires (REL-02).
    pub heartbeat_misses: u32,
    /// Position poll rate; minimum 1 (MNT-02).
    pub poll_hz: u32,
}

impl SerialConfig {
    fn validate(&self, c: &mut Check) {
        c.range("request_timeout_ms", self.request_timeout_ms, 50, 60_000);
        c.range("request_retries", self.request_retries, 0, 10);
        c.range("heartbeat_misses", self.heartbeat_misses, 1, 100);
        // MNT-02 requires at least 1 Hz position telemetry; the upper bound keeps the poll from
        // saturating a link whose measured round trip is ~16.6 ms (SDD §5.2.4).
        c.range("poll_hz", self.poll_hz, 1, 20);
    }
}

/// Rig and tripod dimensions for the Layer 2 collision check (SDD §5.4.3).
///
/// Millimetres throughout, and every one of them a measurement of the operator's own installation.
/// The origin is the intersection of the two mount axes.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RigGeometry {
    /// Distance from the polar axis to the declination axis, along the declination axis — where
    /// the saddle sits. 180 mm on an HEQ5 Pro.
    pub dec_axis_offset_mm: f64,
    /// Half the optical tube's length. The tube is modelled as a capsule centred on the
    /// declination axis, so this is how far each end reaches.
    pub tube_half_length_mm: f64,
    /// The tube's radius, which also inflates the obstacle: a hit is the *surface* touching, not
    /// the centreline.
    pub tube_radius_mm: f64,
    /// How far the optical axis sits off the declination axis — saddle height plus tube radius,
    /// measured **along** the declination axis, continuing `dec_axis_offset_mm` outward: the
    /// saddle stack builds radially away from the mount, so the optical axis crosses the
    /// declination axis's line at the *sum* of the two.
    ///
    /// Not zero on any real rig: the tube lies *on* the saddle, not through it. The direction is
    /// worth stating because the first implementation applied it perpendicular to the axis
    /// instead, which held the modelled tube 230 mm closer to the pier than the metal and refused
    /// a third of the right-ascension turn at dec-home for a rig standing half a metre clear.
    pub saddle_offset_mm: f64,
    /// Height of the intersection of the two mount axes above the ground.
    pub head_height_mm: f64,
    /// How far the tripod's top plate sits *below* the intersection of the two axes.
    ///
    /// The mount body raises the axes above the tripod; without this the legs are modelled as
    /// starting at the axes themselves, which puts them a couple of hundred millimetres too high
    /// and refuses poses that clear them comfortably.
    pub mount_body_height_mm: f64,
    /// Tripod radius at the top, where the legs meet the head — the "cut tip" of the pyramid.
    pub top_radius_mm: f64,
    /// Tripod radius at ground level, i.e. how far the legs splay.
    ///
    /// The obstacle is a truncated **cone**, not the three-sided pyramid the legs actually form:
    /// leg azimuths change at every setup and a stale one is worse than none, so the cone
    /// circumscribes them. It therefore refuses some poses that would have cleared a gap between
    /// two legs, which is the direction to be wrong in.
    pub base_radius_mm: f64,
    /// How far the RA∩DEC crossing sits from the mount's vertical azimuth axis, measured along
    /// the RA shaft toward the sky ("to the front"), in millimetres. Zero if not measured.
    ///
    /// The tripod stands on the *azimuth* axis — the vertical through the top plate — not on
    /// the axes crossing the rest of this geometry is measured from. On a real GEM head the
    /// dec housing rides the sky end of the RA housing, so the crossing hangs forward of the
    /// column: ~60 mm on the operator's mount (2026-08-03, read straight off the rig-model
    /// viewer against the metal). Ignoring it plants the modelled tripod ~35 mm north of the
    /// real one at this latitude and skews every north–south clearance by that much.
    #[serde(default)]
    pub mount_axis_offset_mm: f64,
    /// The angle of the head's boss axis — the casting the RA housing is bolted to — measured
    /// from the base plane, in degrees. 90 (the default) is a vertical column.
    ///
    /// The tripod stands under the point where this axis reaches the base, not under a plumb
    /// line from the RA∩head-axis crossing: on the operator's mount the boss leans at 60°, which
    /// carries the base — and the whole tripod cone — a further ~116 mm away from under the
    /// crossing (2026-08-03, read off the rig-model viewer against the metal).
    #[serde(default = "vertical_head_axis")]
    pub head_axis_angle_degrees: f64,
    /// The counterweight shaft and weights, absent until measured — the same rule as the rig
    /// itself: an unmeasured part gets no limit rather than a guessed one.
    #[serde(default)]
    pub counterweight: Option<CounterweightGeometry>,
    /// The head casting, joint by lettered joint — the rig viewer's vocabulary (A = RA∩DEC,
    /// B = RA∩second boss, C = the bearing joint, D = the main boss's foot, which is where the
    /// tripod stands). When present this supersedes `mount_axis_offset_mm` and
    /// `head_axis_angle_degrees`, which approximated the same casting with a single boss.
    #[serde(default)]
    pub head: Option<HeadChain>,
    /// The camera and focuser stack, absent until measured.
    ///
    /// On the operator's rig it rides the tube's sky end and points along the declination axis,
    /// away from the mount ("parallel to the dec axis, in front") — so it is a capsule of its
    /// own, not a fatter tube: inflating `tube_radius_mm` to cover it swept a full ring the
    /// camera never occupies and refused a third of the RA turn at dec-home for nothing
    /// (2026-08-02/03, the E16 aftermath and the rig-viewer session).
    #[serde(default)]
    pub camera: Option<CameraGeometry>,
}

/// The default head-boss angle: a vertical column, which is also the pre-2026-08-03 model.
fn vertical_head_axis() -> f64 {
    90.0
}

/// The head casting as a chain of lettered joints (SDD §5.4.3; the rig viewer's letters).
///
/// A → B along the RA shaft; B → C down the second boss, perpendicular to the shaft in the
/// meridian, toward the pole side; C sits a fixed height above the base plate; C → D down the
/// main boss at its angle. D is the tripod's centreline. Measured on the operator's mount
/// 2026-08-03, letter by letter against the drawing — and self-checking: the plate depth the
/// chain derives (A→plate ≈ 257 mm on that mount) has to agree with the measured
/// `mount_body_height_mm` (250), which it does to within the tape's honesty.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadChain {
    /// A → B: from the RA∩DEC crossing down the RA shaft to where the second boss meets it.
    pub a_to_b_mm: f64,
    /// B → C: the second boss, perpendicular to the RA shaft, RA housing to the bearing joint.
    pub b_to_c_mm: f64,
    /// C's height above the base plate.
    pub c_above_plate_mm: f64,
    /// C → D: the main boss's angle from the base plane. 90 is a vertical column.
    pub c_d_angle_degrees: f64,
    /// H — the altitude knob: sticks out of the middle of the C→D boss, perpendicular to it,
    /// on the pole side, straight into the under-pier passage the tube and counterweight swing
    /// through. Absent means not modelled, the usual rule.
    #[serde(default)]
    pub alt_knob: Option<AltKnob>,
}

/// The altitude-adjustment knob (H in the rig viewer), a mount-fixed obstacle.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AltKnob {
    /// How far it protrudes from the boss's centreline.
    pub reach_mm: f64,
    /// The knob's radius.
    pub radius_mm: f64,
}

impl HeadChain {
    fn validate(&self, c: &mut Check) {
        c.range_f64("head.a_to_b_mm", self.a_to_b_mm, -500.0, 500.0);
        c.range_f64("head.b_to_c_mm", self.b_to_c_mm, 0.0, 500.0);
        c.range_f64("head.c_above_plate_mm", self.c_above_plate_mm, 0.0, 1000.0);
        c.range_f64("head.c_d_angle_degrees", self.c_d_angle_degrees, 10.0, 90.0);
        if let Some(knob) = self.alt_knob {
            c.range_f64("head.alt_knob.reach_mm", knob.reach_mm, 0.0, 500.0);
            c.range_f64("head.alt_knob.radius_mm", knob.radius_mm, 0.0, 200.0);
        }
    }
}

/// The camera stack, as a capsule off the tube's side (SDD §5.4.3).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraGeometry {
    /// Where the focuser sits along the tube, measured from the declination axis along the
    /// optical axis — positive toward the front (sky) end.
    pub along_tube_mm: f64,
    /// How far the stack reaches from the tube's *surface*, along the declination-axis
    /// direction, away from the mount — the focuser sits on top of the tube, not on its axis,
    /// so "sticks out another 300 mm" is measured from the skin.
    pub reach_mm: f64,
    /// The stack's radius — half the camera body's widest span.
    pub radius_mm: f64,
}

/// The counterweight assembly, modelled as a second capsule opposite the saddle (SDD §5.4.3).
///
/// It swings on the other side of the declination axis and is a real collision hazard during
/// right-ascension motion — 180° of RA at the home pose is zero celestial change and a half-turn
/// of exactly this.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterweightGeometry {
    /// From the declination axis to the shaft's tip, along the shaft.
    pub length_mm: f64,
    /// The largest weight's radius. It inflates the *whole* shaft: weights slide along it, so any
    /// point may carry one, and a stale weight position would be worse than the small
    /// over-refusal.
    pub radius_mm: f64,
}

/// Every field is a length in millimetres and must be positive; `head_height_mm` must also exceed
/// the tube's reach, or the rig is inside the ground before it has moved.
impl RigGeometry {
    fn validate(&self, c: &mut Check) {
        for (name, value) in [
            ("dec_axis_offset_mm", self.dec_axis_offset_mm),
            ("tube_half_length_mm", self.tube_half_length_mm),
            ("tube_radius_mm", self.tube_radius_mm),
            ("saddle_offset_mm", self.saddle_offset_mm),
            ("head_height_mm", self.head_height_mm),
            ("mount_body_height_mm", self.mount_body_height_mm),
            ("top_radius_mm", self.top_radius_mm),
            ("base_radius_mm", self.base_radius_mm),
        ] {
            c.range_f64(name, value, 0.0, 100_000.0);
        }
        if self.base_radius_mm < self.top_radius_mm {
            c.fail(
                "base_radius_mm",
                "a tripod widens toward the ground, so the base radius cannot be smaller than the \
                 top radius — check whether the two have been swapped",
            );
        }
        // A rig that reaches below its own feet before it has moved is a typo, not a limit: the
        // check would refuse every pose and the operator would disable it, which is worse than
        // having no model at all.
        if self.tube_half_length_mm + self.dec_axis_offset_mm >= self.head_height_mm {
            c.fail(
                "head_height_mm",
                "the tube reaches the ground from the home pose — the head height must exceed \
                 dec_axis_offset_mm + tube_half_length_mm, or every slew is refused",
            );
        }
        // Signed, unlike every other length here: a head whose crossing sits *behind* the
        // column is expressible, though nobody has met one. Bounded to something a mount head
        // can physically be.
        c.range_f64("mount_axis_offset_mm", self.mount_axis_offset_mm, -500.0, 500.0);
        // Below ~10° the boss is nearly lying on the plate and the base-reach arithmetic
        // divides by almost nothing; no real casting is built that flat.
        c.range_f64(
            "head_axis_angle_degrees",
            self.head_axis_angle_degrees,
            10.0,
            90.0,
        );
        if let Some(head) = self.head {
            head.validate(c);
        }
        if let Some(cam) = self.camera {
            // `along_tube_mm` is signed — a focuser can sit either side of the dec axis — but it
            // must sit *on* the tube, or the number describes a different rig than the tube does.
            c.range_f64(
                "camera.along_tube_mm",
                cam.along_tube_mm,
                -self.tube_half_length_mm,
                self.tube_half_length_mm,
            );
            c.range_f64("camera.reach_mm", cam.reach_mm, 0.0, 100_000.0);
            c.range_f64("camera.radius_mm", cam.radius_mm, 0.0, 100_000.0);
        }
        if let Some(cw) = self.counterweight {
            c.range_f64("counterweight.length_mm", cw.length_mm, 0.0, 100_000.0);
            c.range_f64("counterweight.radius_mm", cw.radius_mm, 0.0, 100_000.0);
            // The same typo guard as the tube's: a shaft that reaches below the feet at every
            // pose would refuse everything, and the operator would disable the limit.
            if cw.length_mm + cw.radius_mm >= self.head_height_mm {
                c.fail(
                    "counterweight.length_mm",
                    "the counterweight reaches the ground — length_mm + radius_mm must stay \
                     under head_height_mm, or every slew is refused",
                );
            }
        }
    }
}

/// Safety limits and the manual-slew dead-man's switch (PRD §8.1 `mount.limits`).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountLimits {
    /// Reject goto/slew targets below this altitude (MNT-15). SDD §4.4 bounds this to `[0, 45]`.
    pub min_altitude_degrees: f64,
    /// Stop tracking this long past the meridian (MNT-16).
    pub meridian_limit_minutes: f64,
    /// Refuse a manual slew that would drive an axis further than this from the mechanical home
    /// pose, in degrees of axis rotation (M3-T07).
    ///
    /// **A property of the rig's cabling, which is why it is configuration and not a constant.**
    /// Synta motion has no soft limits and nothing else in the system tracked distance from home,
    /// so before this existed an operator on a D-pad could wind an axis indefinitely: 215.6° was
    /// reached on 2026-08-02, and with a telescope, a power lead and a USB cable attached that is
    /// how a cable is torn out or a tube is driven into the pier.
    ///
    /// It bounds *manual* slew only. A goto is bounded by its own target and checked against the
    /// altitude limit; the D-pad has no target at all, which is exactly why it needed a bound.
    ///
    /// One number for both axes rather than two: what it stands for is the shortest lead on the
    /// mount, and no per-axis figure has been measured. If one ever is, this is where it splits.
    pub max_travel_from_home_degrees: f64,
    /// Default authorization window for a manual slew (SDD §5.8.1).
    pub slew_ttl_default_ms: u64,
    /// Server-side clamp on a client-requested TTL (SDD §5.8.1).
    pub slew_ttl_max_ms: u64,
}

impl MountLimits {
    fn validate(&self, c: &mut Check) {
        // The [0, 45] bound is SDD §4.4 verbatim: below 0 disables the limit, above 45 would
        // refuse most of the observable sky.
        c.range_f64("min_altitude_degrees", self.min_altitude_degrees, 0.0, 45.0);
        c.range_f64(
            "meridian_limit_minutes",
            self.meridian_limit_minutes,
            0.0,
            180.0,
        );
        // The ceiling is 180° and it is a fact about the mechanism rather than a policy: an axis
        // counter spans one revolution, so every mechanical pose is reachable within half a turn
        // of home. Travel past 180° is by construction the long way round to somewhere the short
        // way already reaches — wind, never necessity — so above 180 the setting could not refuse
        // anything an operator needs, which makes it not a limit.
        //
        // The floor is a degree rather than zero: zero would refuse every manual slew from the
        // home pose itself and leave the operator no way to move the mount at all.
        c.range_f64(
            "max_travel_from_home_degrees",
            self.max_travel_from_home_degrees,
            1.0,
            180.0,
        );
        // A TTL under 100 ms cannot survive one VPN round trip, so the D-pad would stutter; a
        // TTL over 10 s is no longer a dead-man's switch (SDD §5.8.1, §8.3(3)).
        c.range("slew_ttl_default_ms", self.slew_ttl_default_ms, 100, 10_000);
        c.range("slew_ttl_max_ms", self.slew_ttl_max_ms, 100, 10_000);
        if self.slew_ttl_default_ms > self.slew_ttl_max_ms {
            c.fail(
                "slew_ttl_default_ms",
                format!(
                    "{} exceeds slew_ttl_max_ms ({}) — the default would be clamped away on \
                     every request (SDD §5.8.1)",
                    self.slew_ttl_default_ms, self.slew_ttl_max_ms
                ),
            );
        }
    }
}

/// Camera driver selection (PRD §8.1 `camera.driver`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraDriver {
    /// libgphoto2 bindings (PRD §4.3).
    Gphoto2,
    /// Any INDI-served camera, via `indi_device`.
    Indi,
    /// ASCOM Alpaca.
    AscomAlpaca,
    /// In-process simulator (HAL-11).
    Simulator,
}

impl CameraDriver {
    /// The name this selection resolves to in the HAL driver registry (HAL-07, SDD §5.1).
    ///
    /// See [`MountDriver::as_str`] for why the registry is keyed by these strings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gphoto2 => "gphoto2",
            Self::Indi => "indi",
            Self::AscomAlpaca => "ascom_alpaca",
            Self::Simulator => "simulator",
        }
    }
}

/// A camera operation that may be routed through the `gphoto2` binary instead of the crate
/// bindings (PRD §8.1 `camera.ops_via_cli`, SDD §5.3.3).
///
/// The token set mirrors the `CamCmd` message set in SDD §5.3.1. Ships empty for the R10 — the
/// M2 spike found every operation covered by the bindings, bulb included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraOp {
    /// Open the camera.
    Connect,
    /// Close the camera.
    Disconnect,
    /// Read the settings tree.
    GetSettings,
    /// Write one setting.
    SetSetting,
    /// Timed capture.
    Capture,
    /// Bulb capture (press/hold/release).
    Bulb,
    /// Abort an in-flight capture.
    AbortCapture,
    /// Download a frame off the camera.
    Download,
    /// Live-view preview stream.
    LiveView,
    /// Battery status.
    Battery,
    /// Storage status.
    Storage,
}

/// Camera configuration (PRD §8.1 `camera`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    /// Which driver implementation to build.
    pub driver: CameraDriver,
    /// ISO token as the camera spells it, e.g. `"1600"`.
    pub default_iso: String,
    /// Shutter token as the camera spells it, e.g. `"30"` or `"1/250"`.
    pub default_shutter: String,
    /// Capture format token, e.g. `"RAW+JPEG"`.
    pub default_format: String,
    /// Operations routed through the `gphoto2` binary instead of the bindings (SDD §5.3.3).
    pub ops_via_cli: Vec<CameraOp>,
    /// Operation-class timeouts; a breach declares the camera thread wedged (REL-03).
    pub timeouts: CameraTimeouts,
    /// Live-view frames per second to pull off the camera (PRF-02, USB-11).
    ///
    /// **A ceiling to throttle down to, not a target.** M2-T01 measured the R10 sustaining
    /// **58.5 fps** at 133 KB per frame — 7.8 MB/s of USB and of link, for a preview whose
    /// requirement (PRF-02) is *at least* 5 fps. Chasing the hardware's rate would spend the
    /// wire, the CPU and the Pi's memory bandwidth on frames no operator can see, and USB-11
    /// asks for the opposite: degrade gracefully on a thin link. So the driver paces itself and
    /// this is the knob.
    ///
    /// Defaulted rather than required, because `CameraConfig` is `deny_unknown_fields` and every
    /// deployed `field-node.yaml` predates this key: a required field would refuse to load a
    /// config that is otherwise correct.
    #[serde(default = "default_live_view_fps")]
    pub live_view_fps: u32,
    /// INDI device name; required when `driver: indi`.
    #[serde(default)]
    pub indi_device: Option<String>,
}

/// PRF-02's floor, which is also the right ceiling. See [`CameraConfig::live_view_fps`].
///
/// The same 5 fps the simulator's `CameraProfile` has defaulted to since M1-T06, so the two
/// drivers pace identically and a panel tuned against one is not surprised by the other.
fn default_live_view_fps() -> u32 {
    5
}

impl CameraConfig {
    fn validate(&self, c: &mut Check) {
        c.non_empty("default_iso", &self.default_iso);
        c.non_empty("default_shutter", &self.default_shutter);
        c.non_empty("default_format", &self.default_format);
        c.section("timeouts", |c| self.timeouts.validate(c));
        // The upper bound is the measured hardware rate: a number above it cannot be delivered
        // and would only mean "as fast as possible", which is what the key exists to prevent
        // someone asking for by accident. The lower bound is one frame a second, USB-11's own
        // floor for a degraded link.
        c.range("live_view_fps", self.live_view_fps, 1, 60);

        let mut seen = BTreeSet::new();
        for op in &self.ops_via_cli {
            if !seen.insert(*op) {
                c.fail("ops_via_cli", format!("`{op:?}` is listed more than once"));
            }
        }

        match self.driver {
            CameraDriver::Indi if self.indi_device.is_none() => c.fail(
                "indi_device",
                "required when `camera.driver` is `indi`: name the INDI device, e.g. \"Canon DSLR\"",
            ),
            // PRD §8.1 offers no Alpaca address key under `camera` (only `mount.ascom_host`),
            // so this driver cannot actually be addressed. Refusing at load beats failing at
            // connect with no key to point at. See the M0-T04 result note.
            CameraDriver::AscomAlpaca => c.fail(
                "driver",
                "`ascom_alpaca` cannot be configured: PRD §8.1 defines no Alpaca address key \
                 under `camera`. Use `gphoto2`, `indi` or `simulator`",
            ),
            _ => {}
        }
    }
}

/// Operation-class timeouts (PRD §8.1 `camera.timeouts`, SDD §5.3.1).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraTimeouts {
    /// Get or set one setting.
    pub config_seconds: u64,
    /// Added to the exposure duration to bound a capture.
    pub capture_extra_seconds: u64,
    /// Bound on a frame download.
    pub download_seconds: u64,
}

impl CameraTimeouts {
    fn validate(&self, c: &mut Check) {
        c.range("config_seconds", self.config_seconds, 1, 600);
        c.range("capture_extra_seconds", self.capture_extra_seconds, 1, 3600);
        // SDD §7 bounds the shutdown "finish the in-flight download" step at 120 s; a download
        // timeout beyond an hour would make a wedged camera indistinguishable from a slow one.
        c.range("download_seconds", self.download_seconds, 1, 3600);
    }
}

/// Guide camera driver selection (PRD §8.1 `guide_camera.driver`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideCameraDriver {
    /// ZWO ASI via the ASI SDK.
    Asi,
    /// QHY via the QHY SDK.
    Qhy,
    /// Any INDI-served guide camera.
    Indi,
    /// In-process simulator.
    Simulator,
}

impl GuideCameraDriver {
    /// The name this selection resolves to in the HAL driver registry (HAL-07, SDD §5.1).
    ///
    /// See [`MountDriver::as_str`] for why the registry is keyed by these strings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Asi => "asi",
            Self::Qhy => "qhy",
            Self::Indi => "indi",
            Self::Simulator => "simulator",
        }
    }
}

/// Guide camera (PRD §8.1 `guide_camera`). `driver: null` disables guiding hardware.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuideCameraConfig {
    /// Driver, or `null` for disabled.
    pub driver: Option<GuideCameraDriver>,
    /// ASI enumeration index; optional, defaults to the first camera found.
    #[serde(default)]
    pub asi_index: Option<u32>,
    /// QHY camera id; optional, defaults to the first camera found.
    #[serde(default)]
    pub qhy_id: Option<String>,
    /// INDI device name; required when `driver: indi`.
    #[serde(default)]
    pub indi_device: Option<String>,
}

impl GuideCameraConfig {
    fn validate(&self, c: &mut Check) {
        if let Some(id) = &self.qhy_id {
            c.non_empty("qhy_id", id);
        }
        if let Some(dev) = &self.indi_device {
            c.non_empty("indi_device", dev);
        }
        if self.driver == Some(GuideCameraDriver::Indi) && self.indi_device.is_none() {
            c.fail(
                "indi_device",
                "required when `guide_camera.driver` is `indi`: name the INDI device",
            );
        }
    }
}

/// Guiding parameters (PRD §8.1 `guider`). The Phase 3 keys are optional and inert in Phase 1.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuiderConfig {
    /// Dither offset in guide pixels.
    pub dither_pixels: f64,
    /// Seconds to settle after a dither.
    pub dither_settle: f64,
    /// Guide exposure in seconds (Phase 3).
    #[serde(default)]
    pub exposure: Option<f64>,
    /// RA correction aggressiveness, `(0, 1]` (Phase 3).
    #[serde(default)]
    pub aggressiveness_ra: Option<f64>,
    /// Dec correction aggressiveness, `(0, 1]` (Phase 3).
    #[serde(default)]
    pub aggressiveness_dec: Option<f64>,
}

impl GuiderConfig {
    fn validate(&self, c: &mut Check) {
        c.range_f64("dither_pixels", self.dither_pixels, 0.0, 500.0);
        c.range_f64("dither_settle", self.dither_settle, 0.0, 600.0);
        if let Some(v) = self.exposure {
            c.range_f64("exposure", v, 0.001, 60.0);
        }
        for (name, value) in [
            ("aggressiveness_ra", self.aggressiveness_ra),
            ("aggressiveness_dec", self.aggressiveness_dec),
        ] {
            if let Some(v) = value {
                if !v.is_finite() || !(0.0 < v && v <= 1.0) {
                    c.fail(
                        name,
                        format!("{v} is out of range; expected 0.0 < value <= 1.0"),
                    );
                }
            }
        }
    }
}

/// Plate-solver backend (PRD §8.1 `solver.backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SolverBackend {
    /// ASTAP CLI.
    Astap,
    /// astrometry.net `solve-field`.
    Astrometry,
}

/// Plate solving (PRD §8.1 `solver`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolverConfig {
    /// Which backend to shell out to.
    pub backend: SolverBackend,
    /// Path to the `astap` binary.
    pub astap_path: PathBuf,
    /// Path to the ASTAP star database (e.g. G17).
    pub astap_database: PathBuf,
    /// Path to `astrometry.cfg`.
    pub astrometry_config: PathBuf,
    /// Degrees from the hint position.
    pub search_radius: f64,
    /// Image downsample factor for a faster solve.
    pub downsample: u32,
    /// Solve timeout in seconds.
    pub timeout: u64,
    /// Arcsec — re-slew if the centring offset exceeds this.
    pub center_threshold: f64,
    /// Maximum centring iterations.
    pub center_max_iterations: u32,
}

impl SolverConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("astap_path", &mut self.astap_path);
        c.expand("astap_database", &mut self.astap_database);
        c.expand("astrometry_config", &mut self.astrometry_config);
    }

    fn validate(&self, c: &mut Check) {
        // Existence is deferred to first use (SDD §4.4), but a relative path to a solver binary
        // resolves against whatever directory systemd happened to start us in.
        c.absolute("astap_path", &self.astap_path);
        c.absolute("astap_database", &self.astap_database);
        c.absolute("astrometry_config", &self.astrometry_config);
        c.range_f64("search_radius", self.search_radius, 0.1, 180.0);
        c.range("downsample", self.downsample, 1, 8);
        c.range("timeout", self.timeout, 1, 3600);
        c.range_f64("center_threshold", self.center_threshold, 0.1, 3600.0);
        c.range("center_max_iterations", self.center_max_iterations, 1, 20);
    }
}

/// Equipment profile stamped onto frames for calibration matching (PRD §8.1 `equipment`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EquipmentConfig {
    /// Telescope description, e.g. `"SW 200PDS f/5"`.
    pub telescope: String,
    /// Camera description, e.g. `"Canon R10"`.
    pub camera: String,
    /// Filter description, or `"none"`.
    pub filter: String,
}

impl EquipmentConfig {
    fn validate(&self, c: &mut Check) {
        // These three form the calibration-library match key; an empty one silently widens
        // every dark/flat match.
        c.non_empty("telescope", &self.telescope);
        c.non_empty("camera", &self.camera);
        c.non_empty("filter", &self.filter);
    }
}

/// How frames reach the stacking server (PRD §8.1 `stacking_server.transfer_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferMethod {
    /// POST to the stack node's `/api/ingest` endpoint.
    Http,
    /// rsync to the stack node.
    Rsync,
}

/// Stacking-server link (PRD §8.1 `stacking_server`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackingServerConfig {
    /// Whether to transfer at all. `false` leaves frames on the field node.
    pub enabled: bool,
    /// Stacking server host or IP on the LAN/VPN.
    pub host: String,
    /// Stacking server port.
    pub port: u16,
    /// Upload transport.
    pub transfer_method: TransferMethod,
    /// Seconds between retries when the server is unreachable (backoff base).
    pub retry_interval: u64,
    /// Local spool directory for unsent frames.
    pub queue_dir: PathBuf,
    /// Upload pacing so bulk transfer cannot queue operator commands behind it (SDD §8.3(7)).
    pub pacing: PacingConfig,
}

impl StackingServerConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("queue_dir", &mut self.queue_dir);
    }

    fn validate(&self, c: &mut Check) {
        c.host_name("host", &self.host);
        c.range("port", self.port, 1, u16::MAX);
        c.range("retry_interval", self.retry_interval, 1, 3600);
        c.absolute("queue_dir", &self.queue_dir);
        c.section("pacing", |c| self.pacing.validate(c));
    }
}

/// Transfer pacing (PRD §8.1 `stacking_server.pacing`, SDD §8.3(7)).
///
/// Parsed and validated from M1; enforcement lands with Phase 2b (SDD §5.10.4).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacingConfig {
    /// Upload ceiling in Mbit/s; `null` means uncapped.
    pub bandwidth_cap_mbps: Option<f64>,
    /// Percentage of the cap allowed while the operator is actively commanding.
    pub interactive_floor_pct: f64,
    /// A motion command within this window triggers the floor.
    pub interactive_window_seconds: u64,
}

impl PacingConfig {
    fn validate(&self, c: &mut Check) {
        if let Some(cap) = self.bandwidth_cap_mbps {
            c.range_f64("bandwidth_cap_mbps", cap, 0.001, 100_000.0);
        }
        c.range_f64(
            "interactive_floor_pct",
            self.interactive_floor_pct,
            0.0,
            100.0,
        );
        c.range(
            "interactive_window_seconds",
            self.interactive_window_seconds,
            1,
            3600,
        );
    }
}

/// LLM provider (PRD §8.1 `llm.provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProvider {
    /// Anthropic API.
    Anthropic,
    /// OpenAI API.
    Openai,
    /// Local ollama, via `ollama_host`.
    Ollama,
}

/// What a confirmation tier does (PRD §8.1 `llm.confirmation_tiers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationMode {
    /// Execute immediately.
    Auto,
    /// Require operator confirmation.
    Confirm,
    /// Confirm, with a warning.
    ConfirmWarn,
}

/// Per-tier confirmation policy (PRD §8.1 `llm.confirmation_tiers`). Tier names match the route
/// metadata tiers in SDD §5.8.1.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationTiers {
    /// Read-only routes.
    pub read: ConfirmationMode,
    /// Low-risk state changes.
    pub low: ConfirmationMode,
    /// Motion and capture.
    pub medium: ConfirmationMode,
    /// Park/unpark and other high-consequence actions.
    pub high: ConfirmationMode,
}

/// LLM control layer (PRD §8.1 `llm`). Phase 2c; parsed and validated from Phase 1 so an
/// operator's file never needs trimming.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    /// Whether the control layer is available.
    pub enabled: bool,
    /// Which provider to speak to.
    pub provider: LlmProvider,
    /// Model identifier, pinned at deploy time.
    pub model: String,
    /// Environment variable holding the API key (SEC-04) — never the key itself.
    pub api_key_env: EnvVarName,
    /// Base URL for a local ollama; required when `provider: ollama`.
    #[serde(default)]
    pub ollama_host: Option<String>,
    /// Per-tier confirmation policy.
    pub confirmation_tiers: ConfirmationTiers,
    /// Web Speech API voice commands.
    pub voice_input: bool,
    /// Text-to-speech for responses.
    pub voice_output: bool,
    /// Keep conversation context per session.
    pub session_history: bool,
}

impl LlmConfig {
    fn validate(&self, c: &mut Check) {
        c.non_empty("model", &self.model);
        c.env_var_name("api_key_env", &self.api_key_env);
        match (self.provider, &self.ollama_host) {
            (LlmProvider::Ollama, None) => c.fail(
                "ollama_host",
                "required when `llm.provider` is `ollama`: give the base URL, \
                 e.g. http://localhost:11434",
            ),
            (_, Some(host)) => c.http_url("ollama_host", host),
            _ => {}
        }
    }
}

/// A short deployment name such as `Dev` or `Bench`, shown wherever the operator could otherwise
/// mistake one node for another.
///
/// Newtyped rather than a bare `String` because it reaches two places where junk is expensive: the
/// PWA manifest, which Chrome caches against the installed app, and the header, which has a fixed
/// width. Validation is deliberately narrow — letters, digits, spaces and hyphens — since this
/// string is interpolated into JSON served to a browser, and the set of characters that cannot
/// break out is easier to reason about than the set that can.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct DeploymentLabel(String);

impl DeploymentLabel {
    const MAX: usize = 16;

    /// The label as written in the config.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self, c: &mut Check, name: &str) {
        c.non_empty(name, &self.0);
        if self.0.chars().count() > Self::MAX {
            c.fail(
                name,
                format!(
                    "`{}` is {} characters; {} is the maximum — it is shown on a phone home \
                     screen and in a fixed-width header",
                    self.0,
                    self.0.chars().count(),
                    Self::MAX
                ),
            );
        }
        if let Some(bad) = self
            .0
            .chars()
            .find(|ch| !(ch.is_ascii_alphanumeric() || *ch == ' ' || *ch == '-'))
        {
            c.fail(
                name,
                format!(
                    "`{bad}` is not allowed; a deployment label may contain only ASCII letters, \
                     digits, spaces and hyphens, because it is interpolated into the PWA manifest"
                ),
            );
        }
    }
}

/// Field-node HTTP server (PRD §8.1 `server`).
///
/// Distinct from [`StackServerConfig`]: only the field node carries `max_command_age_ms`,
/// because only the field node initiates motion (SDD §5.8.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldServerConfig {
    /// Bind address; the VPN interface IP in production (SEC-01).
    pub host: String,
    /// Bind port.
    pub port: u16,
    /// Environment variable holding the shared bearer token (SEC-02).
    pub auth_token_env: EnvVarName,
    /// Motion-*initiating* commands older than this are rejected `COMMAND_STALE`; stopping
    /// commands are never age-rejected (SDD §5.8.1).
    pub max_command_age_ms: u64,
    /// Tokio worker threads. `null` means the per-node default from SDD §7 — for the field node
    /// `min(2, cores - 2)` with a floor of 1, deliberately *not* one-per-core, because the
    /// camera OS thread and the decode pool need cores reserved on a 4-core Pi.
    pub runtime_worker_threads: Option<usize>,
    /// Tracing verbosity.
    pub log_level: LogLevel,
    /// Log directory.
    pub log_dir: PathBuf,
    /// Distinguishes a non-production deployment in the PWA's identity and chrome. `null` is
    /// production and adds nothing.
    ///
    /// A PWA's install identity is keyed by *origin*, so a dev node on its own hostname already
    /// installs alongside production and keeps its own token, cache and service worker. What the
    /// origin does not do is make the two distinguishable once installed — two identical icons on
    /// one home screen, one of which drives a real mount. This label is what makes them tell
    /// apart: the manifest name becomes `AstroCtl <label>` and the UI carries a persistent marker.
    pub deployment_label: Option<DeploymentLabel>,
    /// TLS terminated by this process (SEC-05), or `None` for plain HTTP.
    ///
    /// `#[serde(default)]` rather than a required `tls: null`, because **an absent block has to
    /// mean plain HTTP**: `localhost` development and the M0-T08 container harness both run
    /// without a certificate and neither should have to name a key to say so. That is also why
    /// the shipped example carries the block commented out — an operator needs to see the schema,
    /// and a node that ships with TLS half-configured would refuse to start.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

impl FieldServerConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("log_dir", &mut self.log_dir);
        if let Some(tls) = &mut self.tls {
            c.section("tls", |c| tls.normalize(c));
        }
    }

    fn validate(&self, c: &mut Check) {
        c.bind_address("host", &self.host);
        c.range("port", self.port, 1, u16::MAX);
        c.env_var_name("auth_token_env", &self.auth_token_env);
        // Below ~100 ms every command over a VPN would be stale on arrival; above 60 s the
        // rejection stops meaning "the operator's intent has passed" (SDD §5.8.1).
        c.range("max_command_age_ms", self.max_command_age_ms, 100, 60_000);
        check_worker_threads(c, self.runtime_worker_threads);
        c.absolute("log_dir", &self.log_dir);
        if let Some(label) = &self.deployment_label {
            label.validate(c, "deployment_label");
        }
        if let Some(tls) = &self.tls {
            c.section("tls", |c| tls.validate(c));
        }
    }

    /// Worker threads to build the runtime with, resolving `null` to the field-node default
    /// from SDD §7: `min(2, cores - 2)`, floor 1 — never one-per-core.
    #[must_use]
    pub fn resolved_worker_threads(&self, available_cores: usize) -> usize {
        self.runtime_worker_threads
            .unwrap_or_else(|| 2.min(available_cores.saturating_sub(2)).max(1))
    }
}

/// Certificate and key for the TLS the field node terminates itself (PRD §8.1 `server.tls`).
///
/// SEC-05: the operator-facing origin must be a *secure context* or Chrome withholds the Screen
/// Wake Lock API, service-worker registration and `beforeinstallprompt` — USB-09 and USB-10 are
/// unreachable without it, and a VPN does not substitute because the browser judges the origin.
/// ADD §4 puts termination in this process rather than behind a proxy: the field node is the only
/// thing that must be up for the system to work, so it is where the certificate belongs.
///
/// Whether the files are *readable and parseable* is deliberately not checked here. This layer
/// checks shape and range (SDD §4.4); the loader in `astroctl-field` has to read both files
/// anyway, and it can say "the PEM file holds no certificate" where an existence check could only
/// say "missing". Both paths fail startup — SEC-05 is not satisfied by quietly serving plaintext.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain, leaf first — acme.sh's `fullchain.cer`. The chain matters: a leaf
    /// alone validates on a desktop that has cached the intermediate and fails on a phone that
    /// has not, which is the harder failure to reproduce.
    pub cert_path: PathBuf,
    /// PEM private key. PKCS#8, PKCS#1 or SEC1 (`BEGIN EC PRIVATE KEY`) — Let's Encrypt via
    /// acme.sh issues ECDSA by default, and that is a SEC1 key.
    pub key_path: PathBuf,
    /// Days before `notAfter` at which `/api/system/health` degrades to `warn` (SEC-07).
    pub warn_days_before_expiry: u32,
}

impl TlsConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("cert_path", &mut self.cert_path);
        c.expand("key_path", &mut self.key_path);
    }

    fn validate(&self, c: &mut Check) {
        c.absolute("cert_path", &self.cert_path);
        c.absolute("key_path", &self.key_path);
        // Upper bound of 60 because acme.sh renews at 60 days remaining by default: a threshold
        // at or above that is latched on from the moment the renewal window opens, and a warning
        // that is always lit carries no information. 0 is excluded because "warn on the day it
        // expires" is not a warning.
        c.range(
            "warn_days_before_expiry",
            self.warn_days_before_expiry,
            1,
            60,
        );
    }
}

fn check_worker_threads(c: &mut Check, value: Option<usize>) {
    if let Some(n) = value {
        c.range("runtime_worker_threads", n, 1, 1024);
    }
}

// ---------------------------------------------------------------------------------------------
// Stacking server (PRD §8.2)
// ---------------------------------------------------------------------------------------------

/// The whole stacking-server configuration, exactly PRD §8.2.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackConfig {
    /// Stacking method, weighting, registration and rejection parameters.
    pub stacking: StackingConfig,
    /// Mirrored session archive — the authoritative copy (IPP-09, REL-13).
    pub storage: StorageConfig,
    /// Supervised Python compute/ML workers (ADR-13, SDD §5.12.3).
    pub workers: WorkersConfig,
    /// Calibration library.
    pub calibration: CalibrationConfig,
    /// ML models (Phase 4; parsed and validated from Phase 1).
    pub ml: MlConfig,
    /// GPU acceleration budget and toggles.
    pub gpu: GpuConfig,
    /// HTTP server, auth and runtime sizing.
    pub server: StackServerConfig,
}

impl Configuration for StackConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.section("stacking", |c| self.stacking.normalize(c));
        c.section("storage", |c| self.storage.normalize(c));
        c.section("workers", |c| self.workers.normalize(c));
        c.section("calibration", |c| self.calibration.normalize(c));
        c.section("ml", |c| self.ml.normalize(c));
        c.section("server", |c| self.server.normalize(c));
    }

    fn validate(&self, c: &mut Check) {
        c.section("stacking", |c| self.stacking.validate(c));
        c.section("storage", |c| self.storage.validate(c));
        c.section("workers", |c| self.workers.validate(c));
        c.section("calibration", |c| self.calibration.validate(c));
        c.section("ml", |c| self.ml.validate(c));
        c.section("gpu", |c| self.gpu.validate(c));
        c.section("server", |c| self.server.validate(c));
    }
}

impl StackConfig {
    /// The shared bearer token for this node (SEC-02), read from the environment on demand.
    ///
    /// # Errors
    /// [`ConfigError::MissingEnv`] if `server.auth_token_env` names an unset variable.
    pub fn auth_token(&self) -> Result<Secret, ConfigError> {
        self.server.auth_token_env.read("server.auth_token_env")
    }
}

/// Pixel-combination method (PRD §8.2 `stacking.method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackingMethod {
    /// Plain mean.
    Mean,
    /// Mean weighted by frame quality.
    WeightedMean,
    /// Median.
    Median,
    /// Sigma clipping with separate low/high thresholds.
    SigmaClip,
    /// MAD-based kappa-sigma rejection.
    KappaSigma,
    /// Winsorized sigma clipping.
    WinsorizedSigmaClip,
    /// Discard N lowest and N highest per pixel.
    MinMaxClip,
    /// Linear-fit clipping.
    LinearFit,
}

/// Frame weighting (PRD §8.2 `stacking.weight_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeightMode {
    /// All frames equal.
    Equal,
    /// Weight by signal-to-noise ratio.
    Snr,
    /// Weight by star FWHM.
    Fwhm,
    /// Weight by background level.
    Background,
    /// Per-frame weights set in the UI.
    Custom,
}

/// Frame normalization (PRD §8.2 `stacking.normalization`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Normalization {
    /// No normalization.
    None,
    /// Additive to the mean.
    AdditiveMean,
    /// Multiplicative to the mean.
    MultiplicativeMean,
    /// To the median.
    Median,
    /// To a background region.
    BackgroundRegion,
}

/// Registration transform (PRD §8.2 `stacking.registration_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationMethod {
    /// Affine.
    Affine,
    /// Projective.
    Projective,
    /// Translation only.
    TranslationOnly,
}

/// Reference frame selection (PRD §8.2 `stacking.reference_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceMode {
    /// Best quality frame.
    Auto,
    /// The frame named by `reference_frame`.
    Manual,
    /// The first frame of the session.
    First,
}

/// Debayer algorithm (PRD §8.2 `stacking.debayer_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum DebayerMethod {
    /// Bilinear interpolation.
    #[serde(rename = "bilinear")]
    Bilinear,
    /// Variable Number of Gradients.
    #[serde(rename = "VNG")]
    Vng,
    /// Adaptive Homogeneity-Directed.
    #[serde(rename = "AHD")]
    Ahd,
    /// DCB interpolation.
    #[serde(rename = "DCB")]
    Dcb,
}

/// Stacking parameters (PRD §8.2 `stacking`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackingConfig {
    /// Pixel-combination method.
    pub method: StackingMethod,
    /// Sigma-clip / winsorized lower rejection threshold.
    pub sigma_low: f64,
    /// Sigma-clip / winsorized upper rejection threshold.
    pub sigma_high: f64,
    /// Kappa-sigma rejection threshold (MAD-based).
    pub kappa: f64,
    /// Maximum iterative-rejection passes.
    pub max_iterations: u32,
    /// Min/max clip: discard N lowest per pixel.
    pub clip_low: u32,
    /// Min/max clip: discard N highest per pixel.
    pub clip_high: u32,
    /// Use a running approximation for live stacking; full re-stack on demand.
    pub live_approximation: bool,
    /// Frame weighting.
    pub weight_mode: WeightMode,
    /// Frame normalization.
    pub normalization: Normalization,
    /// Registration transform.
    pub registration_method: RegistrationMethod,
    /// Minimum detected stars to attempt registration.
    pub min_star_count: u32,
    /// Pixels — reject registration if the RMS residual exceeds this.
    pub max_residual: f64,
    /// `sep` detection threshold, sigma above background.
    pub detection_threshold: f64,
    /// Reference frame selection.
    pub reference_mode: ReferenceMode,
    /// Frame number, when `reference_mode: manual`.
    pub reference_frame: Option<u64>,
    /// Debayer algorithm.
    pub debayer_method: DebayerMethod,
    /// Arcsec — reject frames with FWHM above this.
    pub reject_fwhm_max: f64,
    /// Reject frames with fewer detected stars.
    pub reject_star_count_min: u32,
    /// Reject frames with star eccentricity above this (trailing).
    pub reject_eccentricity_max: f64,
    /// ADU — reject frames with background above this (clouds); `null` disables.
    pub reject_background_max: Option<f64>,
    /// Where stacks are exported.
    pub export_dir: PathBuf,
}

impl StackingConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("export_dir", &mut self.export_dir);
    }

    fn validate(&self, c: &mut Check) {
        c.range_f64("sigma_low", self.sigma_low, 0.1, 10.0);
        c.range_f64("sigma_high", self.sigma_high, 0.1, 10.0);
        c.range_f64("kappa", self.kappa, 0.1, 10.0);
        c.range("max_iterations", self.max_iterations, 1, 100);
        c.range("clip_low", self.clip_low, 0, 100);
        c.range("clip_high", self.clip_high, 0, 100);
        c.range("min_star_count", self.min_star_count, 3, 100_000);
        c.range_f64("max_residual", self.max_residual, 0.01, 100.0);
        c.range_f64("detection_threshold", self.detection_threshold, 0.1, 100.0);
        c.range_f64("reject_fwhm_max", self.reject_fwhm_max, 0.1, 60.0);
        c.range(
            "reject_star_count_min",
            self.reject_star_count_min,
            0,
            100_000,
        );
        c.range_f64(
            "reject_eccentricity_max",
            self.reject_eccentricity_max,
            0.0,
            1.0,
        );
        if let Some(v) = self.reject_background_max {
            c.range_f64("reject_background_max", v, 0.0, 1e9);
        }
        c.absolute("export_dir", &self.export_dir);

        if self.reference_mode == ReferenceMode::Manual && self.reference_frame.is_none() {
            c.fail(
                "reference_frame",
                "required when `stacking.reference_mode` is `manual`: give the frame number",
            );
        }
        if self.method == StackingMethod::MinMaxClip && self.clip_low == 0 && self.clip_high == 0 {
            c.fail(
                "clip_low",
                "`min_max_clip` with clip_low and clip_high both 0 rejects nothing; \
                 use `mean` instead or raise one of them",
            );
        }
    }
}

/// Supervised Python worker processes (PRD §8.2 `workers`, ADR-13, SDD §5.12.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersConfig {
    /// The venv interpreter the workers run in. Pinned deliberately (PRD §7).
    pub python_interpreter: PathBuf,
    /// Compute worker script.
    pub compute_worker: PathBuf,
    /// ML worker script.
    pub ml_worker: PathBuf,
    /// Ping interval; three consecutive misses kill and restart the worker.
    pub health_ping_seconds: u64,
    /// Base of the capped exponential restart backoff (60 s ceiling, SDD §5.12.3).
    pub restart_backoff_seconds: u64,
    /// A job running longer than this is cancelled, then the worker is killed.
    pub job_timeout_seconds: u64,
}

impl WorkersConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("python_interpreter", &mut self.python_interpreter);
        c.expand("compute_worker", &mut self.compute_worker);
        c.expand("ml_worker", &mut self.ml_worker);
    }

    fn validate(&self, c: &mut Check) {
        // The interpreter must be absolute: which Python the supervisor spawns is the single
        // thing about the worker environment that must not depend on the working directory.
        c.absolute("python_interpreter", &self.python_interpreter);
        // The worker scripts are deliberately allowed to stay relative (PRD §8.2 ships
        // `workers/compute_worker.py`), so only emptiness is an error here.
        for (name, path) in [
            ("compute_worker", &self.compute_worker),
            ("ml_worker", &self.ml_worker),
        ] {
            if path.as_os_str().is_empty() {
                c.fail(name, "must not be empty");
            }
        }
        c.range("health_ping_seconds", self.health_ping_seconds, 1, 300);
        // The backoff is capped at 60 s (SDD §5.12.3), so a base above the cap is meaningless.
        c.range(
            "restart_backoff_seconds",
            self.restart_backoff_seconds,
            1,
            60,
        );
        c.range("job_timeout_seconds", self.job_timeout_seconds, 1, 86_400);

        // Three missed pings kill the worker (SDD §5.12.3). A job timeout inside that window
        // means the supervisor can never distinguish "slow job" from "dead worker".
        let liveness_window = self.health_ping_seconds.saturating_mul(3);
        if self.job_timeout_seconds <= liveness_window {
            c.fail(
                "job_timeout_seconds",
                format!(
                    "{} must exceed 3 x health_ping_seconds ({liveness_window}) — inside the \
                     liveness window a slow job is indistinguishable from a dead worker \
                     (SDD §5.12.3)",
                    self.job_timeout_seconds
                ),
            );
        }
    }
}

/// Calibration library (PRD §8.2 `calibration`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationConfig {
    /// Library root.
    pub library_dir: PathBuf,
    /// Index file name inside `library_dir`, e.g. `library.json`.
    pub index_file: String,
    /// °C — match darks within this temperature range.
    pub dark_temp_tolerance: f64,
    /// Flag masters older than this for re-acquisition.
    pub dark_max_age_days: u32,
    /// Method used to generate master frames.
    pub default_master_method: StackingMethod,
    /// Recommended minimum sub-frames per master.
    pub default_master_sub_count: u32,
}

impl CalibrationConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("library_dir", &mut self.library_dir);
    }

    fn validate(&self, c: &mut Check) {
        c.absolute("library_dir", &self.library_dir);
        c.non_empty("index_file", &self.index_file);
        if self.index_file.contains('/') {
            c.fail(
                "index_file",
                format!(
                    "`{}` must be a bare file name inside library_dir, not a path",
                    self.index_file
                ),
            );
        }
        c.range_f64("dark_temp_tolerance", self.dark_temp_tolerance, 0.1, 50.0);
        c.range("dark_max_age_days", self.dark_max_age_days, 1, 3650);
        c.range(
            "default_master_sub_count",
            self.default_master_sub_count,
            1,
            10_000,
        );
    }
}

/// ML inference device (PRD §8.2 `ml.device`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MlDevice {
    /// GPU if available, else CPU.
    Auto,
    /// CUDA.
    Cuda,
    /// CPU.
    Cpu,
}

/// One opt-in ML model (PRD §8.2 `ml.<stage>`, MLR-07).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MlModel {
    /// Model name inside `ml.models_dir`.
    pub model: String,
    /// Opt-in switch; defaults to `false` in the shipped example (MLR-07).
    pub enabled: bool,
}

impl MlModel {
    fn validate(&self, c: &mut Check) {
        c.non_empty("model", &self.model);
    }
}

/// ML processing (PRD §8.2 `ml`). Phase 4; parsed and validated from Phase 1.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MlConfig {
    /// Where model files live.
    pub models_dir: PathBuf,
    /// Inference device.
    pub device: MlDevice,
    /// Noise reduction model.
    pub noise_reduction: MlModel,
    /// Star separation model.
    pub star_separation: MlModel,
    /// Background extraction model.
    pub background_extraction: MlModel,
    /// Reference images per target.
    pub reference_library_dir: PathBuf,
}

impl MlConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("models_dir", &mut self.models_dir);
        c.expand("reference_library_dir", &mut self.reference_library_dir);
    }

    fn validate(&self, c: &mut Check) {
        c.absolute("models_dir", &self.models_dir);
        c.absolute("reference_library_dir", &self.reference_library_dir);
        c.section("noise_reduction", |c| self.noise_reduction.validate(c));
        c.section("star_separation", |c| self.star_separation.validate(c));
        c.section("background_extraction", |c| {
            self.background_extraction.validate(c);
        });
    }
}

/// Which stages run on the GPU (PRD §8.2 `gpu.accelerate`).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuAccelerate {
    /// GPU star detection and warp.
    pub registration: bool,
    /// GPU sigma-clip / rejection.
    pub accumulation: bool,
    /// GPU VNG/AHD debayer.
    pub debayer: bool,
    /// GPU stretch, curves, colour ops.
    pub post_processing: bool,
    /// ML models on GPU.
    pub ml_inference: bool,
}

/// GPU budget and toggles (PRD §8.2 `gpu`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuConfig {
    /// Whether GPU acceleration is used at all.
    pub enabled: bool,
    /// `auto`, `cpu`, `cuda` or `cuda:N`.
    pub device: String,
    /// VRAM the pipeline may use, leaving headroom for the OS/display.
    pub vram_budget_gb: f64,
    /// Per-stage toggles.
    pub accelerate: GpuAccelerate,
}

impl GpuConfig {
    fn validate(&self, c: &mut Check) {
        // PRD §8.2 documents "auto", "cuda:0" or "cpu" here, so unlike `ml.device` this key is
        // device-indexed and cannot be a closed enum.
        let d = self.device.as_str();
        let ok = d == "auto"
            || d == "cpu"
            || d == "cuda"
            || d.strip_prefix("cuda:")
                .is_some_and(|i| !i.is_empty() && i.chars().all(|ch| ch.is_ascii_digit()));
        if !ok {
            c.fail(
                "device",
                format!(
                    "`{d}` is not a device selector; expected `auto`, `cpu`, `cuda` or `cuda:N`"
                ),
            );
        }
        c.range_f64("vram_budget_gb", self.vram_budget_gb, 0.1, 1024.0);
    }
}

/// Stacking-server HTTP server (PRD §8.2 `server`).
///
/// Deliberately not the same struct as [`FieldServerConfig`]: PRD §8.2 has no
/// `max_command_age_ms`, because the stack node initiates no motion.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackServerConfig {
    /// Bind address; the VPN interface IP in production (SEC-01).
    pub host: String,
    /// Bind port.
    pub port: u16,
    /// Environment variable holding the shared bearer token (SEC-02).
    pub auth_token_env: EnvVarName,
    /// Tokio worker threads. `null` means the per-node default from SDD §7 — one per core here,
    /// because the heavy compute lives in child processes with their own scheduling.
    pub runtime_worker_threads: Option<usize>,
    /// Tracing verbosity.
    pub log_level: LogLevel,
    /// Log directory.
    pub log_dir: PathBuf,
}

impl StackServerConfig {
    fn normalize(&mut self, c: &mut Check) {
        c.expand("log_dir", &mut self.log_dir);
    }

    fn validate(&self, c: &mut Check) {
        c.bind_address("host", &self.host);
        c.range("port", self.port, 1, u16::MAX);
        c.env_var_name("auth_token_env", &self.auth_token_env);
        check_worker_threads(c, self.runtime_worker_threads);
        c.absolute("log_dir", &self.log_dir);
    }

    /// Worker threads to build the runtime with, resolving `null` to the stack-node default
    /// from SDD §7: one per core.
    #[must_use]
    pub fn resolved_worker_threads(&self, available_cores: usize) -> usize {
        self.runtime_worker_threads
            .unwrap_or_else(|| available_cores.max(1))
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const PRD: &str = include_str!("../../../docs/intent/ASTROCTL-PRD-001.md");
    const FIELD_EXAMPLE: &str = include_str!("../../../config/field-node.example.yaml");
    const STACK_EXAMPLE: &str = include_str!("../../../config/stacking-server.example.yaml");

    /// Pull the fenced YAML block that follows a heading out of the PRD.
    fn prd_yaml_block(heading: &str) -> String {
        let after = PRD
            .split_once(heading)
            .unwrap_or_else(|| panic!("PRD has no heading `{heading}`"))
            .1;
        let block = after
            .split_once("```yaml\n")
            .expect("heading is followed by a yaml fence")
            .1;
        let body = block.split_once("\n```").expect("fence is closed").0;
        format!("{body}\n")
    }

    fn field(yaml: &str) -> Result<Arc<FieldConfig>, ConfigError> {
        FieldConfig::from_yaml(yaml, "field-node.yaml")
    }

    fn stack(yaml: &str) -> Result<Arc<StackConfig>, ConfigError> {
        StackConfig::from_yaml(yaml, "stacking-server.yaml")
    }

    /// Drop a top-level section (its key line and every indented line under it) from a YAML doc.
    fn without_section(yaml: &str, section: &str) -> String {
        let mut out = String::new();
        let mut skipping = false;
        for line in yaml.lines() {
            let top_level_key = !line.starts_with([' ', '\t', '#']) && line.contains(':');
            if skipping {
                if line.is_empty() || line.starts_with([' ', '\t']) {
                    continue;
                }
                skipping = false;
            }
            if top_level_key && line.starts_with(&format!("{section}:")) {
                skipping = true;
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    // --- the drift guard -----------------------------------------------------------------

    /// Report the *first differing line* rather than dumping both 100-line documents — the
    /// point of this guard is to be readable at the moment someone trips it.
    fn assert_verbatim(shipped_name: &str, shipped: &str, prd_heading: &str) {
        let prd = prd_yaml_block(prd_heading);
        if shipped == prd {
            return;
        }
        for (n, (a, b)) in shipped.lines().zip(prd.lines()).enumerate() {
            assert_eq!(
                a,
                b,
                "{shipped_name} has drifted from PRD {prd_heading} at line {}.\n\
                 Copy the PRD block over the example file, or fix the PRD — do not \
                 reconcile them by editing the structs.",
                n + 1
            );
        }
        panic!(
            "{shipped_name} has drifted from PRD {prd_heading}: it has {} lines, the PRD block \
             has {}",
            shipped.lines().count(),
            prd.lines().count()
        );
    }

    #[test]
    fn shipped_field_example_is_the_prd_block_verbatim() {
        assert_verbatim(
            "config/field-node.example.yaml",
            FIELD_EXAMPLE,
            "### 8.1 Field Node Configuration",
        );
    }

    #[test]
    fn shipped_stack_example_is_the_prd_block_verbatim() {
        assert_verbatim(
            "config/stacking-server.example.yaml",
            STACK_EXAMPLE,
            "### 8.2 Stacking Server Configuration",
        );
    }

    #[test]
    fn prd_field_example_loads_and_validates_unchanged() {
        let cfg = field(FIELD_EXAMPLE).expect("PRD §8.1 example must load");

        // Spot-check the keys the design depends on, so a rename cannot pass silently.
        assert_eq!(cfg.mount.limits.slew_ttl_default_ms, 500);
        assert_eq!(cfg.mount.limits.slew_ttl_max_ms, 2000);
        assert_eq!(cfg.server.max_command_age_ms, 2000);
        assert_eq!(cfg.server.runtime_worker_threads, None);
        assert!(cfg.camera.ops_via_cli.is_empty());
        assert_eq!(cfg.camera.timeouts.download_seconds, 120);
        assert_eq!(cfg.mount.serial.heartbeat_misses, 3);
        assert_eq!(cfg.mount.serial.poll_hz, 1);
        assert_eq!(
            cfg.storage.sessions_dir,
            PathBuf::from("/data/astro/sessions")
        );
        assert!((cfg.storage.disk_critical_free_gb - 5.0).abs() < f64::EPSILON);
        assert_eq!(cfg.stacking_server.pacing.bandwidth_cap_mbps, None);
        assert_eq!(cfg.stacking_server.pacing.interactive_window_seconds, 10);
        assert_eq!(cfg.server.auth_token_env.name(), "ASTROCTL_TOKEN");
        assert_eq!(cfg.llm.api_key_env.name(), "ANTHROPIC_API_KEY");
        assert_eq!(cfg.guide_camera.driver, None);
    }

    #[test]
    fn prd_stack_example_loads_and_validates_unchanged() {
        let cfg = stack(STACK_EXAMPLE).expect("PRD §8.2 example must load");

        assert_eq!(cfg.workers.health_ping_seconds, 5);
        assert_eq!(cfg.workers.restart_backoff_seconds, 2);
        assert_eq!(cfg.workers.job_timeout_seconds, 300);
        assert_eq!(
            cfg.workers.python_interpreter,
            PathBuf::from("/data/astro/venv/bin/python")
        );
        assert_eq!(
            cfg.workers.compute_worker,
            PathBuf::from("workers/compute_worker.py")
        );
        assert_eq!(cfg.server.runtime_worker_threads, None);
        assert_eq!(cfg.stacking.method, StackingMethod::SigmaClip);
        assert_eq!(cfg.stacking.debayer_method, DebayerMethod::Vng);
        assert_eq!(cfg.stacking.reference_frame, None);
        assert_eq!(cfg.stacking.reject_background_max, None);
        assert_eq!(cfg.ml.device, MlDevice::Auto);
        assert!(!cfg.ml.noise_reduction.enabled);
        assert_eq!(cfg.gpu.device, "auto");
        assert_eq!(cfg.server.port, 8471);
    }

    /// SDD §7: `null` is the *per-node* default, and on the field node that default is
    /// deliberately not one-per-core.
    #[test]
    fn null_worker_threads_resolves_to_the_per_node_default() {
        let f = field(FIELD_EXAMPLE).expect("loads");
        assert_eq!(f.server.resolved_worker_threads(4), 2, "4-core Pi: not 4");
        assert_eq!(f.server.resolved_worker_threads(16), 2, "capped at 2");
        assert_eq!(f.server.resolved_worker_threads(2), 1, "floor of 1");
        assert_eq!(f.server.resolved_worker_threads(1), 1, "floor of 1");

        let s = stack(STACK_EXAMPLE).expect("loads");
        assert_eq!(
            s.server.resolved_worker_threads(16),
            16,
            "stack: one per core"
        );
    }

    // --- server.tls (SEC-05, M0-T09) ------------------------------------------------------

    /// The one property everything else depends on: no block means plain HTTP. `localhost`
    /// development and the M0-T08 container harness both ship configs derived from this example,
    /// and a required `tls:` key would break both.
    #[test]
    fn the_shipped_example_configures_no_tls_and_still_loads() {
        let f = field(FIELD_EXAMPLE).expect("loads");
        assert!(
            f.server.tls.is_none(),
            "the example must document the block without enabling it"
        );
    }

    fn with_tls(body: &str) -> String {
        // `server` is the last section of the example, so appending lands inside it.
        format!("{FIELD_EXAMPLE}  tls:\n{body}")
    }

    #[test]
    fn a_tls_block_is_parsed_whole() {
        let yaml = with_tls(
            "    cert_path: /etc/astroctl/tls/fullchain.pem\n\
             \x20   key_path: /etc/astroctl/tls/privkey.pem\n\
             \x20   warn_days_before_expiry: 21\n",
        );
        let tls = field(&yaml)
            .expect("a well-formed tls block loads")
            .server
            .tls
            .clone()
            .expect("the block is present");
        assert_eq!(
            tls.cert_path,
            PathBuf::from("/etc/astroctl/tls/fullchain.pem")
        );
        assert_eq!(tls.key_path, PathBuf::from("/etc/astroctl/tls/privkey.pem"));
        assert_eq!(tls.warn_days_before_expiry, 21);
    }

    #[test]
    fn a_typo_inside_the_tls_block_is_rejected_like_any_other() {
        let yaml = with_tls(
            "    cert_path: /etc/astroctl/tls/fullchain.pem\n\
             \x20   key_path: /etc/astroctl/tls/privkey.pem\n\
             \x20   warn_days_before_expiry: 14\n\
             \x20   warn_days: 14\n",
        );
        let err = field(&yaml).expect_err("an unknown key inside tls must not be accepted");
        assert!(err.to_string().contains("warn_days"), "{err}");
    }

    /// A relative path resolves against whatever directory systemd happened to start the unit in,
    /// which is the class of bug that only appears in production.
    #[test]
    fn a_relative_certificate_path_is_rejected_naming_the_key() {
        let yaml = with_tls(
            "    cert_path: tls/fullchain.pem\n\
             \x20   key_path: /etc/astroctl/tls/privkey.pem\n\
             \x20   warn_days_before_expiry: 14\n",
        );
        let err = field(&yaml).expect_err("a relative path must not be accepted");
        let text = err.to_string();
        assert!(text.contains("server.tls.cert_path"), "no path in: {text}");
        assert!(text.contains("absolute"), "unclear message: {text}");
    }

    #[test]
    fn a_warning_threshold_that_could_never_switch_off_is_rejected() {
        let yaml = with_tls(
            "    cert_path: /etc/astroctl/tls/fullchain.pem\n\
             \x20   key_path: /etc/astroctl/tls/privkey.pem\n\
             \x20   warn_days_before_expiry: 120\n",
        );
        let err = field(&yaml).expect_err("120 days outlives acme.sh's renewal window");
        let text = err.to_string();
        assert!(
            text.contains("server.tls.warn_days_before_expiry"),
            "no path in: {text}"
        );
        assert!(text.contains("1..=60"), "no expected range in: {text}");
    }

    #[test]
    fn tls_paths_get_the_same_tilde_expansion_as_every_other_path() {
        let yaml = with_tls(
            "    cert_path: ~/tls/fullchain.pem\n\
             \x20   key_path: ~/tls/privkey.pem\n\
             \x20   warn_days_before_expiry: 14\n",
        );
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return; // No HOME: `expand` reports its own error, covered by expand_tilde's tests.
        };
        let tls = field(&yaml)
            .expect("`~` expands")
            .server
            .tls
            .clone()
            .expect("present");
        assert_eq!(tls.cert_path, home.join("tls/fullchain.pem"));
        assert_eq!(tls.key_path, home.join("tls/privkey.pem"));
    }

    #[test]
    fn a_configuration_that_still_names_a_park_position_is_refused() {
        // M3-T07's third acceptance criterion: "a configuration whose park position cannot be
        // expressed is rejected at load rather than silently under-constraining an axis".
        //
        // It cannot be expressed *at all* now, which is the strongest form of that: park drives
        // both counters to `0x800000` and there is no value an operator could write that would
        // change where it goes. `deny_unknown_fields` therefore refuses the key outright, and an
        // operator upgrading an old file finds out at load rather than at the pier.
        let yaml = FIELD_EXAMPLE.replace(
            "  settle_time_seconds: 3",
            "  park_position:\n    ra_hours: 0.0\n    dec_degrees: 90.0\n  settle_time_seconds: 3",
        );
        let err = field(&yaml).expect_err("a park position must not be accepted");
        let text = err.to_string();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
        assert!(
            text.contains("park_position"),
            "no offending key in: {text}"
        );
        assert!(text.contains("unknown field"), "unclear message: {text}");
    }

    // --- fixture: unknown key ------------------------------------------------------------

    #[test]
    fn unknown_key_is_rejected_and_names_its_yaml_path() {
        let yaml = FIELD_EXAMPLE.replace(
            "    slew_ttl_max_ms: 2000",
            "    slew_ttl_max_ms: 2000\n    slew_ttl_maximum_ms: 2000",
        );
        let err = field(&yaml).expect_err("a typo must not be accepted");
        let text = err.to_string();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
        assert!(text.contains("mount.limits"), "no YAML path in: {text}");
        assert!(
            text.contains("slew_ttl_maximum_ms"),
            "no offending key in: {text}"
        );
        assert!(text.contains("unknown field"), "unclear message: {text}");
    }

    #[test]
    fn unknown_top_level_section_is_rejected() {
        let yaml = format!("{FIELD_EXAMPLE}\ntelescope_control:\n  enabled: true\n");
        let err = field(&yaml).expect_err("unknown section must not be accepted");
        assert!(err.to_string().contains("telescope_control"), "{err}");
    }

    #[test]
    fn unknown_enum_variant_is_rejected_with_the_valid_set() {
        let yaml = FIELD_EXAMPLE.replace("driver: skywatcher", "driver: skywatchr");
        let err = field(&yaml).expect_err("a misspelled driver must not be accepted");
        let text = err.to_string();
        assert!(text.contains("skywatchr"), "{text}");
        assert!(
            text.contains("simulator"),
            "expected set not listed: {text}"
        );
    }

    #[test]
    fn unknown_camera_op_is_rejected() {
        let yaml = FIELD_EXAMPLE.replace("ops_via_cli: []", "ops_via_cli: [blub]");
        let err = field(&yaml).expect_err("an unknown camera op must not be accepted");
        let text = err.to_string();
        assert!(text.contains("camera.ops_via_cli"), "no path in: {text}");
        assert!(text.contains("blub"), "{text}");
    }

    // --- fixture: out-of-range value -----------------------------------------------------

    #[test]
    fn out_of_range_value_is_rejected_and_names_its_yaml_path() {
        let yaml = FIELD_EXAMPLE.replace("min_altitude_degrees: 15", "min_altitude_degrees: 60");
        let err = field(&yaml).expect_err("60 deg is outside the SDD §4.4 bound");
        let text = err.to_string();
        assert!(matches!(err, ConfigError::Invalid { .. }), "got {err:?}");
        assert!(
            text.contains("mount.limits.min_altitude_degrees"),
            "no YAML path in: {text}"
        );
        assert!(text.contains("0..=45"), "no expected range in: {text}");
    }

    #[test]
    fn nan_is_not_silently_accepted_as_in_range() {
        let yaml = FIELD_EXAMPLE.replace("min_altitude_degrees: 15", "min_altitude_degrees: .nan");
        let err = field(&yaml).expect_err("NaN passes every comparison and must be caught");
        assert!(
            err.to_string()
                .contains("mount.limits.min_altitude_degrees"),
            "{err}"
        );
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let yaml = FIELD_EXAMPLE
            .replace("min_altitude_degrees: 15", "min_altitude_degrees: 60")
            .replace("baud: 9600", "baud: 9601")
            .replace("port: 8470", "port: 0");
        let err = field(&yaml).expect_err("three bad values");
        let ConfigError::Invalid { errors, .. } = &err else {
            panic!("expected Invalid, got {err:?}");
        };
        let keys: Vec<&str> = errors.iter().map(|e| e.key.as_str()).collect();
        assert!(
            keys.contains(&"mount.limits.min_altitude_degrees"),
            "{keys:?}"
        );
        assert!(keys.contains(&"mount.baud"), "{keys:?}");
        assert!(keys.contains(&"server.port"), "{keys:?}");
        assert!(
            err.to_string().starts_with("3 configuration errors"),
            "{err}"
        );
    }

    // --- fixture: missing required section ------------------------------------------------

    #[test]
    fn missing_required_section_is_rejected_and_names_it() {
        let yaml = without_section(FIELD_EXAMPLE, "storage");
        assert!(
            !yaml.contains("sessions_dir"),
            "fixture did not remove the section"
        );
        let err = field(&yaml).expect_err("storage is required (REL-12)");
        let text = err.to_string();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
        assert!(text.contains("storage"), "no key named in: {text}");
        assert!(text.contains("missing field"), "unclear message: {text}");
    }

    #[test]
    fn missing_nested_key_names_the_enclosing_path() {
        let yaml = FIELD_EXAMPLE.replace("    download_seconds: 120", "");
        let err = field(&yaml).expect_err("camera.timeouts.download_seconds is required");
        let text = err.to_string();
        assert!(text.contains("camera.timeouts"), "no YAML path in: {text}");
        assert!(text.contains("download_seconds"), "{text}");
    }

    #[test]
    fn missing_stack_section_is_rejected_and_names_it() {
        let yaml = without_section(STACK_EXAMPLE, "workers");
        let err = stack(&yaml).expect_err("workers is required (SDD §5.12.3)");
        assert!(err.to_string().contains("workers"), "{err}");
    }

    // --- cross-field rules ----------------------------------------------------------------

    #[test]
    fn slew_ttl_default_above_max_is_rejected() {
        let yaml = FIELD_EXAMPLE.replace("slew_ttl_default_ms: 500", "slew_ttl_default_ms: 3000");
        let err = field(&yaml).expect_err("a default above the clamp is meaningless");
        let text = err.to_string();
        assert!(text.contains("mount.limits.slew_ttl_default_ms"), "{text}");
        assert!(text.contains("slew_ttl_max_ms"), "{text}");
    }

    #[test]
    fn disk_critical_above_warn_is_rejected() {
        let yaml = FIELD_EXAMPLE.replace("disk_critical_free_gb: 5", "disk_critical_free_gb: 50");
        let err = field(&yaml).expect_err("the warning must arrive before the stop");
        assert!(
            err.to_string().contains("storage.disk_critical_free_gb"),
            "{err}"
        );
    }

    #[test]
    fn indi_driver_without_a_device_name_is_rejected() {
        let yaml = FIELD_EXAMPLE.replace("driver: skywatcher", "driver: indi");
        let err = field(&yaml).expect_err("indi needs a device name");
        assert!(err.to_string().contains("mount.indi_device"), "{err}");
    }

    #[test]
    fn commented_out_optional_keys_are_still_part_of_the_schema() {
        let yaml = FIELD_EXAMPLE.replace(
            "driver: skywatcher",
            "driver: indi\n  indi_device: \"EQMod Mount\"",
        );
        let cfg = field(&yaml).expect("uncommenting a documented optional key must work");
        assert_eq!(cfg.mount.indi_device.as_deref(), Some("EQMod Mount"));
    }

    #[test]
    fn manual_reference_mode_without_a_frame_is_rejected() {
        let yaml = STACK_EXAMPLE.replace("reference_mode: auto", "reference_mode: manual");
        let err = stack(&yaml).expect_err("manual mode needs a frame number");
        assert!(
            err.to_string().contains("stacking.reference_frame"),
            "{err}"
        );
    }

    #[test]
    fn job_timeout_inside_the_liveness_window_is_rejected() {
        let yaml = STACK_EXAMPLE.replace("job_timeout_seconds: 300", "job_timeout_seconds: 10");
        let err = stack(&yaml).expect_err("10 s is inside 3 x 5 s of ping misses");
        let text = err.to_string();
        assert!(text.contains("workers.job_timeout_seconds"), "{text}");
        assert!(text.contains("health_ping_seconds"), "{text}");
    }

    #[test]
    fn a_non_ip_bind_address_is_rejected() {
        let yaml = FIELD_EXAMPLE.replace("host: 0.0.0.0", "host: my-pi.local");
        let err = field(&yaml).expect_err("a bind address must be an IP literal (SEC-01)");
        assert!(err.to_string().contains("server.host"), "{err}");
    }

    #[test]
    fn a_url_where_a_host_belongs_is_rejected() {
        let yaml = FIELD_EXAMPLE.replace("host: 192.168.1.100", "host: http://192.168.1.100:8471");
        let err = field(&yaml).expect_err("stacking_server.host takes a bare host");
        assert!(err.to_string().contains("stacking_server.host"), "{err}");
    }

    // --- path expansion --------------------------------------------------------------------

    #[test]
    fn tilde_paths_expand_against_home() {
        let home = PathBuf::from("/home/astro");
        assert_eq!(
            expand_tilde(Path::new("~/data/sessions"), Some(&home)),
            Ok(PathBuf::from("/home/astro/data/sessions"))
        );
        assert_eq!(expand_tilde(Path::new("~"), Some(&home)), Ok(home.clone()));
        assert_eq!(
            expand_tilde(Path::new("/data/astro"), Some(&home)),
            Ok(PathBuf::from("/data/astro"))
        );
        assert!(
            expand_tilde(Path::new("~/data"), None).is_err(),
            "no HOME must fail loudly"
        );
        assert!(
            expand_tilde(Path::new("~someone/data"), Some(&home)).is_err(),
            "~user is not supported"
        );
    }

    #[test]
    fn a_relative_directory_is_rejected_with_its_path() {
        let yaml =
            FIELD_EXAMPLE.replace("sessions_dir: /data/astro/sessions", "sessions_dir: data");
        let err = field(&yaml).expect_err("a relative session root depends on the cwd");
        let text = err.to_string();
        assert!(text.contains("storage.sessions_dir"), "{text}");
        assert!(text.contains("absolute"), "{text}");
    }

    // --- secrets ---------------------------------------------------------------------------

    #[test]
    fn auth_token_value_never_appears_in_debug_output() {
        const TOKEN: &str = "tok_live_9f3c1b7a-do-not-log-me";
        std::env::set_var("ASTROCTL_TEST_TOKEN_REDACTION", TOKEN);

        let yaml = FIELD_EXAMPLE.replace(
            "auth_token_env: ASTROCTL_TOKEN",
            "auth_token_env: ASTROCTL_TEST_TOKEN_REDACTION",
        );
        let cfg = field(&yaml).expect("loads");

        // The struct holds the variable *name*, and Debug says so without reaching for a value.
        let dumped = format!("{cfg:?}");
        assert!(
            !dumped.contains(TOKEN),
            "token leaked into config Debug output"
        );
        assert!(dumped.contains("ASTROCTL_TEST_TOKEN_REDACTION"), "{dumped}");
        assert!(dumped.contains("<redacted>"), "{dumped}");

        // And the resolved secret is redacted in both Debug and Display.
        let secret = cfg.auth_token().expect("variable is set");
        assert_eq!(secret.expose(), TOKEN);
        assert!(
            !format!("{secret:?}").contains(TOKEN),
            "token leaked into Secret Debug"
        );
        assert!(
            !format!("{secret}").contains(TOKEN),
            "token leaked into Secret Display"
        );
        assert_eq!(format!("{secret}"), "<redacted>");

        std::env::remove_var("ASTROCTL_TEST_TOKEN_REDACTION");
    }

    #[test]
    fn an_unset_token_variable_names_both_the_key_and_the_variable() {
        let yaml = FIELD_EXAMPLE.replace(
            "auth_token_env: ASTROCTL_TOKEN",
            "auth_token_env: ASTROCTL_DEFINITELY_UNSET_VARIABLE",
        );
        let cfg = field(&yaml).expect("loads");
        let err = cfg.auth_token().expect_err("variable is not set");
        let text = err.to_string();
        assert!(text.contains("server.auth_token_env"), "{text}");
        assert!(
            text.contains("ASTROCTL_DEFINITELY_UNSET_VARIABLE"),
            "{text}"
        );
    }

    #[test]
    fn an_invalid_env_var_name_is_rejected() {
        let yaml = FIELD_EXAMPLE.replace(
            "auth_token_env: ASTROCTL_TOKEN",
            "auth_token_env: \"astroctl token\"",
        );
        let err = field(&yaml).expect_err("a name the shell cannot export must be refused");
        assert!(err.to_string().contains("server.auth_token_env"), "{err}");
    }

    // --- I/O -------------------------------------------------------------------------------

    #[test]
    fn a_missing_file_names_the_path_it_tried() {
        let err = load_field_config("/nonexistent/astroctl/field-node.yaml")
            .expect_err("missing file must fail");
        assert!(matches!(err, ConfigError::Io { .. }), "got {err:?}");
        assert!(
            err.to_string()
                .contains("/nonexistent/astroctl/field-node.yaml"),
            "{err}"
        );
    }

    // --- driver names (HAL-07) -------------------------------------------------------------

    #[test]
    fn driver_names_are_the_spelling_the_operator_writes() {
        // The HAL registry (SDD §5.1) is keyed by `as_str()`. If it ever stopped agreeing with
        // what serde accepts, a valid `field-node.yaml` would fail at startup with "no mount
        // driver named `…`" — naming a driver that is registered. Assert the round trip on
        // every variant, in YAML, because YAML is what the operator actually writes.
        for driver in [
            MountDriver::Skywatcher,
            MountDriver::Indi,
            MountDriver::AscomAlpaca,
            MountDriver::Simulator,
        ] {
            let name = driver.as_str();
            assert_eq!(
                yaml_serde::from_str::<MountDriver>(name).expect("name deserializes"),
                driver,
                "mount driver `{name}`"
            );
        }
        for driver in [
            CameraDriver::Gphoto2,
            CameraDriver::Indi,
            CameraDriver::AscomAlpaca,
            CameraDriver::Simulator,
        ] {
            let name = driver.as_str();
            assert_eq!(
                yaml_serde::from_str::<CameraDriver>(name).expect("name deserializes"),
                driver,
                "camera driver `{name}`"
            );
        }
        for driver in [
            GuideCameraDriver::Asi,
            GuideCameraDriver::Qhy,
            GuideCameraDriver::Indi,
            GuideCameraDriver::Simulator,
        ] {
            let name = driver.as_str();
            assert_eq!(
                yaml_serde::from_str::<GuideCameraDriver>(name).expect("name deserializes"),
                driver,
                "guide camera driver `{name}`"
            );
        }

        // And the example config's own selections resolve, which is the path startup takes.
        let cfg = field(FIELD_EXAMPLE).expect("loads");
        assert_eq!(cfg.mount.driver.as_str(), "skywatcher");
        assert_eq!(cfg.camera.driver.as_str(), "gphoto2");
    }
}
