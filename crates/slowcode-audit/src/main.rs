//! `slowcode-audit`: render one offline audit report from a session journal.
//!
//! The tool is read-only. It never modifies the journal and never calls a
//! model.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

fn main() -> ExitCode {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => {
                let Some(path) = args.next() else {
                    eprintln!("error: --out needs a path");
                    return ExitCode::from(2);
                };
                output = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                usage();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("error: unknown argument {other}");
                usage();
                return ExitCode::from(2);
            }
            other => {
                if input.is_some() {
                    eprintln!("error: one session journal per run");
                    return ExitCode::from(2);
                }
                input = Some(PathBuf::from(other));
            }
        }
    }

    let Some(input) = input else {
        usage();
        return ExitCode::from(2);
    };
    if let Some(output) = &output
        && same_file(input.as_path(), output.as_path())
    {
        eprintln!("error: --out must not overwrite the session journal");
        return ExitCode::from(2);
    }

    let view = match slowcode_audit::load_session(&input) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let report = slowcode_audit::render_report(&view, &slowcode_audit::signals(&view));

    if let Some(output) = output {
        if let Err(error) = fs::write(&output, report) {
            eprintln!("error: cannot write report: {error}");
            return ExitCode::FAILURE;
        }
    } else {
        print!("{report}");
    }
    ExitCode::SUCCESS
}

fn same_file(input: &Path, output: &Path) -> bool {
    match (fs::canonicalize(input), fs::canonicalize(output)) {
        (Ok(a), Ok(b)) => a == b,
        _ => input == output,
    }
}

fn usage() {
    eprintln!("usage: slowcode-audit <session.jsonl> [--out report.md]");
    eprintln!();
    eprintln!("Reads one session journal and prints a rule-based audit report.");
    eprintln!("The journal is never modified and no model is called.");
}
