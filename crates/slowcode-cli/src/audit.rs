//! `slowcode audit` rebuilds the offline workflow audit site.
//!
//! It is a recovery and migration entry point. Normal operation updates the
//! site after every workflow commit; this command reruns the same pure
//! projection for one workflow or every workflow under `.scratch/workflows`.

use std::path::PathBuf;

use slowcode_app::workflow::audit_site::{self, RebuildOutcome};

const USAGE: &str = "usage: slowcode audit rebuild [WORKFLOW_ID|--all] [--workspace PATH]";

#[derive(Debug, Default)]
struct Options {
    workflow_id: Option<String>,
    all: bool,
    workspace: Option<PathBuf>,
}

/// Run the audit subcommand.
pub fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let options = parse_args(args)?;
    let workspace = options.workspace.unwrap_or_else(|| PathBuf::from("."));
    let target = if options.all {
        None
    } else {
        options.workflow_id.as_deref()
    };
    let outcome = audit_site::rebuild(&workspace, target).map_err(|error| error.to_string())?;
    report(&outcome);
    if outcome.failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} workflow(s) could not be rebuilt; see above",
            outcome.failures.len()
        ))
    }
}

fn report(outcome: &RebuildOutcome) {
    if outcome.reports.is_empty() && outcome.failures.is_empty() {
        println!("no workflows found under .scratch/workflows");
        return;
    }
    for report in &outcome.reports {
        let sources = report.sources;
        println!(
            "{} ({}) sources: state {} / events {} / session {} / feedback {}",
            report.dir.join("index.html").display(),
            report.files.len(),
            mark(sources.state),
            mark(sources.events),
            mark(sources.session),
            mark(sources.feedback),
        );
    }
    for (id, error) in &outcome.failures {
        eprintln!("failed: {id}: {error}");
    }
}

fn mark(ok: bool) -> &'static str {
    if ok { "ok" } else { "missing" }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Err(USAGE.to_owned());
    };
    if command != "rebuild" {
        return Err(format!("unknown audit command `{command}`\n\n{USAGE}"));
    }
    let mut options = Options::default();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--all" => options.all = true,
            "--workspace" | "-w" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value after --workspace\n\n{USAGE}"))?;
                options.workspace = Some(PathBuf::from(value));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`\n\n{USAGE}"));
            }
            other => {
                if options.workflow_id.is_some() {
                    return Err(format!("only one workflow id is accepted\n\n{USAGE}"));
                }
                options.workflow_id = Some(other.to_owned());
            }
        }
    }
    if options.all && options.workflow_id.is_some() {
        return Err(format!(
            "choose a workflow id or --all, not both\n\n{USAGE}"
        ));
    }
    if !options.all && options.workflow_id.is_none() {
        return Err(format!("name a workflow id or pass --all\n\n{USAGE}"));
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn one_workflow_and_a_workspace_are_parsed() {
        let options =
            parse_args(args(&["rebuild", "plan-1", "--workspace", "/tmp/ws"])).expect("parse");
        assert_eq!(options.workflow_id.as_deref(), Some("plan-1"));
        assert!(!options.all);
        assert_eq!(options.workspace, Some(PathBuf::from("/tmp/ws")));
    }

    #[test]
    fn all_is_accepted_but_conflicts_with_an_id() {
        assert!(parse_args(args(&["rebuild", "--all"])).expect("all").all);
        assert!(parse_args(args(&["rebuild", "--all", "plan-1"])).is_err());
    }

    #[test]
    fn a_target_is_required() {
        assert!(parse_args(args(&["rebuild"])).is_err());
        assert!(parse_args(args(&["status"])).is_err());
    }
}
