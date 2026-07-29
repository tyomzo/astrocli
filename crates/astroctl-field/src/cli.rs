//! Command line for `astroctl-field`.
//!
//! Hand-rolled rather than `clap`: three options and no subcommands do not justify a dependency
//! with a build-time cost on the Pi, and the parser is small enough to test exhaustively.

use std::ffi::OsString;
use std::path::PathBuf;

/// Where the configuration is read from when `--config` is not given.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/astroctl/field-node.yaml";

/// Environment variable that overrides [`DEFAULT_CONFIG_PATH`]. Container-friendly: the M0-T08
/// images set it instead of baking a path into the entrypoint.
pub const CONFIG_PATH_ENV: &str = "ASTROCTL_FIELD_CONFIG";

/// What the process was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Run the node with this configuration file.
    Run {
        /// Path to the field-node YAML.
        config: PathBuf,
    },
    /// Print usage and exit 0.
    Help,
    /// Print the version and exit 0.
    Version,
    /// The arguments made no sense; the string is the operator-facing reason.
    Invalid(String),
}

/// Usage text, printed by `--help` and by any argument error.
pub const USAGE: &str = "\
astroctl-field — AstroCtl field node (mount, camera, API, PWA)

USAGE:
    astroctl-field [--config <PATH>]

OPTIONS:
    -c, --config <PATH>   Field-node configuration file (PRD §8.1).
                          Default: $ASTROCTL_FIELD_CONFIG, else /etc/astroctl/field-node.yaml
    -h, --help            Print this help and exit
    -V, --version         Print the version and exit

Hardware is never connected at startup — connecting the mount or camera is an explicit
operator action through the API (SDD §8.1).";

/// Parse arguments, excluding `argv[0]`.
pub fn parse<I>(args: I, config_env: Option<OsString>) -> Invocation
where
    I: IntoIterator<Item = OsString>,
{
    let mut config: Option<PathBuf> = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => return Invocation::Help,
            Some("-V" | "--version") => return Invocation::Version,
            Some("-c" | "--config") => {
                match args.next() {
                    Some(path) => config = Some(PathBuf::from(path)),
                    None => return Invocation::Invalid(
                        "`--config` needs a path, e.g. `--config /etc/astroctl/field-node.yaml`"
                            .to_owned(),
                    ),
                }
            }
            Some(other) if other.starts_with("--config=") => {
                config = Some(PathBuf::from(&other["--config=".len()..]));
            }
            _ => {
                return Invocation::Invalid(format!(
                    "unexpected argument `{}`",
                    arg.to_string_lossy()
                ))
            }
        }
    }

    Invocation::Run {
        config: config
            .or_else(|| config_env.map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn run_config(invocation: Invocation) -> PathBuf {
        match invocation {
            Invocation::Run { config } => config,
            other => panic!("expected a run invocation, got {other:?}"),
        }
    }

    #[test]
    fn config_precedence_is_flag_then_env_then_default() {
        assert_eq!(
            run_config(parse(
                args(&["--config", "/a.yaml"]),
                Some("/b.yaml".into())
            )),
            PathBuf::from("/a.yaml")
        );
        assert_eq!(
            run_config(parse(args(&[]), Some("/b.yaml".into()))),
            PathBuf::from("/b.yaml")
        );
        assert_eq!(
            run_config(parse(args(&[]), None)),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
    }

    #[test]
    fn both_config_spellings_work() {
        assert_eq!(
            run_config(parse(args(&["-c", "/a.yaml"]), None)),
            PathBuf::from("/a.yaml")
        );
        assert_eq!(
            run_config(parse(args(&["--config=/a.yaml"]), None)),
            PathBuf::from("/a.yaml")
        );
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(args(&["--help"]), None), Invocation::Help);
        assert_eq!(parse(args(&["-h"]), None), Invocation::Help);
        assert_eq!(parse(args(&["--version"]), None), Invocation::Version);
        assert_eq!(parse(args(&["-V"]), None), Invocation::Version);
    }

    #[test]
    fn bad_arguments_are_named_not_ignored() {
        assert!(matches!(
            parse(args(&["--config"]), None),
            Invocation::Invalid(_)
        ));
        match parse(args(&["--auto-connect"]), None) {
            Invocation::Invalid(message) => {
                assert!(message.contains("--auto-connect"), "{message}")
            }
            other => panic!("an unknown flag must not be silently ignored, got {other:?}"),
        }
    }
}
