use std::process::ExitCode;

use slowcode_app::{RuntimeConfig, SessionChoice};
use slowcode_cli::{CliOptions, Command, USAGE, parse_args};
use slowcode_tui::TuiConfig;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return start_tui(parse_options(Vec::new())).await;
    }

    match args[0].as_str() {
        "tui" => {
            args.remove(0);
            start_tui(parse_options(args)).await
        }
        "run" => {
            args.remove(0);
            match parse_options(args) {
                Ok(options) if options.prompt.is_some() => run_headless(options).await,
                Ok(_) => {
                    eprintln!("error: `slowcode run` requires a prompt\n\n{USAGE}");
                    ExitCode::from(2)
                }
                Err(error) => usage_error(error),
            }
        }
        "repl" => {
            args.remove(0);
            match parse_options(args) {
                Ok(mut options) => {
                    options.prompt = None;
                    run_headless(options).await
                }
                Err(error) => usage_error(error),
            }
        }
        "feedback" => {
            args.remove(0);
            match slowcode_cli::feedback::run(args) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("error: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => match parse_options(args) {
            Ok(options) if options.prompt.is_some() => run_headless(options).await,
            Ok(options) => start_tui(Ok(options)).await,
            Err(error) => usage_error(error),
        },
    }
}

fn parse_options(args: Vec<String>) -> Result<CliOptions, String> {
    match parse_args(args)? {
        Command::Help => {
            print!("{USAGE}");
            std::process::exit(0);
        }
        Command::Version => {
            println!("slowcode {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Command::Run(options) => Ok(options),
    }
}

async fn start_tui(options: Result<CliOptions, String>) -> ExitCode {
    let options = match options {
        Ok(options) => options,
        Err(error) => return usage_error(error),
    };
    let session = session_choice(&options);
    let config = TuiConfig {
        runtime: RuntimeConfig {
            workspace: options.workspace,
            model: options.model,
            read_only: options.read_only,
            max_tokens: options.max_tokens,
            context_window: options.context_window,
        },
        session,
        ask: options.ask,
    };
    match slowcode_tui::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_headless(options: CliOptions) -> ExitCode {
    let _ = dotenvy::dotenv();
    let _ = dotenvy::from_path(options.workspace.join(".env"));
    let status = slowcode_app::credential_status(&options.workspace);
    if !status.configured() {
        eprintln!(
            "error: no DeepSeek API key is configured\n\
             \n\
             Fix it with any of these:\n\
             \x20 1. run `slowcode` and type /login to save the key to {}\n\
             \x20 2. export DEEPSEEK_API_KEY=<key>\n\
             \x20 3. add DEEPSEEK_API_KEY=<key> to {}",
            status.env_path.display(),
            status.env_path.display()
        );
        return ExitCode::from(2);
    }
    match slowcode_cli::run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let message = error.to_string();
            eprintln!("error: {message}");
            if message.contains("approval_required") {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn session_choice(options: &CliOptions) -> SessionChoice {
    match (&options.session, options.resume) {
        (Some(id), true) => SessionChoice::Resume(id.clone()),
        (Some(id), false) if id == "new" => SessionChoice::New,
        (Some(id), false) => SessionChoice::Create(id.clone()),
        (None, _) => SessionChoice::New,
    }
}

fn usage_error(error: String) -> ExitCode {
    eprintln!("error: {error}\n\n{USAGE}");
    ExitCode::from(2)
}
