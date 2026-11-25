use std::{env, path::PathBuf, process::ExitCode};

use slowcode_docs::{build_site, check_site, validate};

fn usage() -> ExitCode {
    eprintln!(
        "usage: slowcode-docs <check|build [--check]>\n\n\
         check        validate docs.json, tasks.json and the roadmap main flow\n\
         build        validate and regenerate site/dist\n\
         build --check  fail when site/dist is stale"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let result = match arguments.as_slice() {
        [command] if command == "check" => validate(&root).inspect(|()| {
            println!("docs 校验通过：层级、任务源、主流程一致");
        }),
        [command] if command == "build" => build_site(&root).map(|files| {
            println!("写入 {} 个文件到 site/dist", files.len());
        }),
        [command, flag] if command == "build" && flag == "--check" => {
            check_site(&root).inspect(|()| {
                println!("site/dist 与源文件一致");
            })
        }
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::FAILURE
        }
    }
}
