//! `slowcode update` command.
//!
//! `--check` only reads the release metadata (through the 24 hour cache).
//! Without `--check`, the command downloads the asset for this host, verifies
//! the sha256 file, and replaces the running executable. Nothing happens
//! without this explicit command; the TUI asks before calling the same path.

use slowcode_update::{Environment, InstallOutcome, UpdateError, UpdateInfo};

pub const UPDATE_USAGE: &str = "\
slowcode update [--check]

Checks the latest GitHub release for the configured repository
(`SLOWCODE_REPO`, default milocooper74575/slowcode).

Options:
      --check   Report the current and latest version; do not download
  -h, --help    Show this help

Without --check the updater downloads slowcode-<target>.tar.gz and its
.sha256, verifies the checksum, and replaces the running executable. It
refuses Homebrew-managed installs and asks you to restart afterwards.
Set SLOWCODE_UPDATE_URL only for tests; the default is HTTPS GitHub releases.";

/// What `slowcode update` was asked to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateMode {
    Check,
    Install,
    Help,
}

/// Parse arguments after the `update` subcommand.
pub fn parse_update_args<I>(args: I) -> Result<UpdateMode, String>
where
    I: IntoIterator<Item = String>,
{
    let mut mode = UpdateMode::Install;
    let mut seen = false;
    for argument in args {
        match argument.as_str() {
            "--check" => {
                if seen {
                    return Err("--check given more than once".to_owned());
                }
                mode = UpdateMode::Check;
                seen = true;
            }
            "-h" | "--help" => return Ok(UpdateMode::Help),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(mode)
}

/// Run a parsed update command and print its report.
pub async fn run(mode: UpdateMode) -> Result<(), UpdateError> {
    let environment = Environment::from_process();
    match mode {
        UpdateMode::Check => {
            let info = slowcode_update::check_now(&environment).await?;
            print!(
                "{}",
                check_report(&info, environment.pinned_version.as_deref())
            );
        }
        UpdateMode::Install => {
            println!("checking the latest release");
            let outcome = slowcode_update::install_now(&environment).await?;
            print!("{}", install_report(&outcome));
        }
        UpdateMode::Help => println!("{UPDATE_USAGE}"),
    }
    Ok(())
}

/// Render `--check`.
#[must_use]
pub fn check_report(info: &UpdateInfo, pinned: Option<&str>) -> String {
    let mut text = format!("current: {}\nlatest:  {}\n", info.current, info.latest);
    if info.is_available() {
        text.push_str("update available: run `slowcode update`\n");
    } else {
        text.push_str(&format!("slowcode is up to date ({})\n", info.current));
    }
    if let Some(pin) = pinned.map(str::trim).filter(|pin| !pin.is_empty()) {
        text.push_str(&format!(
            "pinned: SLOWCODE_VERSION={pin}; update prompts are off and `slowcode update` refuses until it is unset\n"
        ));
    }
    text
}

/// Render an install attempt.
#[must_use]
pub fn install_report(outcome: &InstallOutcome) -> String {
    match outcome {
        InstallOutcome::UpToDate { current } => {
            format!("slowcode is up to date ({current})\n")
        }
        InstallOutcome::Updated { path, old, new } => format!(
            "updated {}\n{} -> {}\nrestart to use it\n",
            path.display(),
            old,
            new
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use slowcode_update::{InstallOutcome, UpdateInfo, Version};

    use super::{UpdateMode, check_report, install_report, parse_update_args};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_check_and_rejects_noise() {
        assert_eq!(
            parse_update_args(args(&["--check"])).expect("check"),
            UpdateMode::Check
        );
        assert_eq!(
            parse_update_args(args(&[])).expect("install"),
            UpdateMode::Install
        );
        assert_eq!(
            parse_update_args(args(&["--help"])).expect("help"),
            UpdateMode::Help
        );
        assert!(parse_update_args(args(&["--force"])).is_err());
        assert!(parse_update_args(args(&["--check", "--check"])).is_err());
    }

    #[test]
    fn check_report_says_when_an_update_is_available() {
        let info = UpdateInfo {
            current: Version::parse("0.1.0").expect("version"),
            latest: Version::parse("0.2.0").expect("version"),
        };
        let report = check_report(&info, None);
        assert!(report.contains("current: 0.1.0"));
        assert!(report.contains("latest:  0.2.0"));
        assert!(report.contains("run `slowcode update`"));

        let pinned = check_report(&info, Some("v0.1.0"));
        assert!(pinned.contains("pinned: SLOWCODE_VERSION=v0.1.0"));
    }

    #[test]
    fn check_report_says_when_current() {
        let info = UpdateInfo {
            current: Version::parse("0.2.0").expect("version"),
            latest: Version::parse("0.2.0").expect("version"),
        };
        assert!(check_report(&info, None).contains("up to date (0.2.0)"));
    }

    #[test]
    fn install_report_tells_the_user_to_restart() {
        let outcome = InstallOutcome::Updated {
            path: PathBuf::from("/home/me/.local/bin/slowcode"),
            old: Version::parse("0.1.0").expect("version"),
            new: Version::parse("0.2.0").expect("version"),
        };
        let report = install_report(&outcome);
        assert!(report.contains("0.1.0 -> 0.2.0"));
        assert!(report.contains("restart to use it"));
        assert!(
            !install_report(&InstallOutcome::UpToDate {
                current: Version::parse("0.2.0").expect("version"),
            })
            .contains("restart")
        );
    }
}
