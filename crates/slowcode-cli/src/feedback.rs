//! `slowcode feedback`: package one durable session as an issue-ready bundle.

use std::path::PathBuf;

use slowcode_feedback::{BundleRequest, write_bundle};
use slowcode_kernel::session::{SessionStore, validate_session_id};

const USAGE: &str =
    "usage: slowcode feedback <session-id> [--out DIR] [--plan PATH] [--title TEXT] [--note TEXT]";

#[derive(Debug, Default)]
struct Options {
    session_id: Option<String>,
    out_dir: Option<PathBuf>,
    plan: Option<PathBuf>,
    title: Option<String>,
    note: Option<String>,
    free: Vec<String>,
}

/// Run the feedback subcommand. A non-zero result means the bundle is missing
/// or failed the hard completeness gate.
pub fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let options = parse_args(args)?;
    let session_id = options.session_id.ok_or_else(|| USAGE.to_owned())?;
    let id = validate_session_id(&session_id).map_err(|error| error.to_string())?;
    let out_dir = options
        .out_dir
        .unwrap_or_else(|| PathBuf::from("feedback").join(&session_id));

    let store = SessionStore::open_default().map_err(|error| error.to_string())?;
    let journal = store.read_journal(&id).map_err(|error| error.to_string())?;
    let os = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let outcome = write_bundle(BundleRequest {
        session_id: &session_id,
        journal: &journal,
        workspace: None,
        version: env!("CARGO_PKG_VERSION"),
        model: None,
        provider: None,
        os: &os,
        out_dir,
        title: options.title,
        note: options.note,
        plan: options.plan,
    })
    .map_err(|error| error.to_string())?;

    println!("{}", outcome.out_dir.join("issue.md").display());
    if outcome.complete {
        return Ok(());
    }
    let reasons = outcome.hard_failures.join("; ");
    Err(format!(
        "feedback bundle INCOMPLETE ({}): {reasons}\nreview issue.md before posting",
        outcome.out_dir.display()
    ))
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .ok_or_else(|| format!("missing value after {flag}\n{USAGE}"))
        };
        match argument.as_str() {
            "--out" => options.out_dir = Some(PathBuf::from(value("--out")?)),
            "--plan" => options.plan = Some(PathBuf::from(value("--plan")?)),
            "--title" => options.title = Some(value("--title")?),
            "--note" => options.note = Some(value("--note")?),
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other if other.starts_with("--") => {
                return Err(format!("unknown argument: {other}\n{USAGE}"));
            }
            other => set_session(&mut options, other)?,
        }
    }
    if !options.free.is_empty() {
        if options.note.is_some() {
            return Err(format!("use either free text or --note, not both\n{USAGE}"));
        }
        options.note = Some(options.free.join(" "));
    }
    Ok(options)
}

fn set_session(options: &mut Options, value: &str) -> Result<(), String> {
    if options.session_id.is_none() {
        options.session_id = Some(value.to_owned());
        return Ok(());
    }
    options.free.push(value.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn free_text_after_the_session_id_becomes_the_note() {
        let options = parse_args(words(&[
            "session-33333333-3333-4333-8333-333333333333",
            "plan",
            "did",
            "not",
            "pause",
        ]))
        .expect("options");
        assert_eq!(options.note.as_deref(), Some("plan did not pause"));
    }

    #[test]
    fn free_text_and_note_flag_are_mutually_exclusive() {
        let error = parse_args(words(&[
            "session-33333333-3333-4333-8333-333333333333",
            "--note",
            "flag",
            "free",
        ]))
        .expect_err("conflict");
        assert!(error.contains("use either free text or --note"), "{error}");
    }
}
