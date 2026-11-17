//! Real DeepSeek end-to-end test. Ignored by default; run with:
//!
//! ```bash
//! cargo test -p slowcode-cli --test e2e_deepseek -- --ignored --nocapture
//! ```

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/slowcode-cli is two levels below the repo root")
        .to_owned()
}

fn temp_workspace() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("slowcode-e2e-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

#[test]
#[ignore = "requires DEEPSEEK_API_KEY and makes a real API call"]
fn read_write_bash_end_to_end() {
    let root = repo_root();
    let _ = dotenvy::from_path(root.join(".env"));
    assert!(
        std::env::var_os("DEEPSEEK_API_KEY").is_some(),
        "DEEPSEEK_API_KEY is missing; put it in {} or export it",
        root.join(".env").display()
    );

    let workspace = temp_workspace();
    let hello = workspace.join("hello.txt");
    fs::write(&hello, "hello\n").expect("fixture is written");

    let prompt = "Read hello.txt. Append a new line containing exactly goodbye to it. \
                  Then use the bash tool to run `wc -l hello.txt > result.txt`. \
                  Finish with a one-line summary.";

    let output = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("--workspace")
        .arg(&workspace)
        .arg("--model")
        .arg(std::env::var("SLOWCODE_E2E_MODEL").unwrap_or_else(|_| "deepseek-flash".to_owned()))
        .arg("--max-tokens")
        .arg("2048")
        .arg(prompt)
        .output()
        .expect("slowcode binary runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "slowcode failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let hello_after = fs::read_to_string(&hello).expect("hello.txt is still readable");
    assert!(
        hello_after.contains("goodbye"),
        "agent did not append goodbye; file was:\n{hello_after}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let result = workspace.join("result.txt");
    let result_after = fs::read_to_string(&result).unwrap_or_default();
    assert!(
        result_after.contains('2'),
        "wc result did not contain 2; result.txt was:\n{result_after}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    println!("stdout:\n{stdout}");
    println!("stderr:\n{stderr}");
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
#[ignore = "requires DEEPSEEK_API_KEY and makes a real API call"]
fn plan_produces_a_table_and_pauses() {
    let root = repo_root();
    let _ = dotenvy::from_path(root.join(".env"));
    assert!(
        std::env::var_os("DEEPSEEK_API_KEY").is_some(),
        "DEEPSEEK_API_KEY is missing; put it in {} or export it",
        root.join(".env").display()
    );

    let workspace = temp_workspace();
    fs::write(workspace.join("notes.txt"), "alpha\nbeta\n").expect("fixture is written");

    let prompt = "/plan 为 notes.txt 增加一个统计行数的函数，并说明哪些核心逻辑需要用户自己实现。";
    let output = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("--workspace")
        .arg(&workspace)
        .arg("--model")
        .arg(std::env::var("SLOWCODE_E2E_MODEL").unwrap_or_else(|_| "deepseek-flash".to_owned()))
        .arg("--max-tokens")
        .arg("32768")
        .arg(prompt)
        .output()
        .expect("slowcode binary runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "/plan failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let plan_directory = workspace.join(".scratch/plans");
    let entries: Vec<_> = fs::read_dir(&plan_directory)
        .unwrap_or_else(|error| panic!("plan directory missing: {error}\nstdout:\n{stdout}"))
        .collect::<Result<_, _>>()
        .expect("plan entries are readable");
    assert_eq!(entries.len(), 1, "expected exactly one plan artifact");
    let artifact = fs::read_to_string(entries[0].path()).expect("artifact is readable");
    assert!(artifact.contains("认知任务分配表"), "{artifact}");
    assert!(artifact.contains("status: paused"), "{artifact}");
    assert!(artifact.contains("validation: ok"), "{artifact}");

    println!("stdout:\n{stdout}");
    println!("stderr:\n{stderr}");
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
#[ignore = "requires DEEPSEEK_API_KEY and makes two real API calls"]
fn plan_new_pauses_the_previous_workflow() {
    let root = repo_root();
    let _ = dotenvy::from_path(root.join(".env"));
    assert!(
        std::env::var_os("DEEPSEEK_API_KEY").is_some(),
        "DEEPSEEK_API_KEY is missing; put it in {} or export it",
        root.join(".env").display()
    );

    let workspace = temp_workspace();
    fs::write(workspace.join("notes.txt"), "alpha\nbeta\n").expect("fixture is written");

    let run = |prompt: &str| {
        Command::new(env!("CARGO_BIN_EXE_slowcode"))
            .arg("--workspace")
            .arg(&workspace)
            .arg("--model")
            .arg(
                std::env::var("SLOWCODE_E2E_MODEL").unwrap_or_else(|_| "deepseek-flash".to_owned()),
            )
            .arg("--max-tokens")
            .arg("32768")
            .arg(prompt)
            .output()
            .expect("slowcode binary runs")
    };

    let first =
        run("/plan 为 notes.txt 增加一个统计行数的函数，并说明哪些核心逻辑需要用户自己实现。");
    let stdout = String::from_utf8_lossy(&first.stdout);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first.status.success(),
        "first /plan failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let index_path = workspace.join(".scratch/workflows/index.json");
    let index: serde_json::Value =
        serde_json::from_slice(&fs::read(&index_path).expect("index is readable"))
            .expect("index is JSON");
    let old_id = index["active"]
        .as_str()
        .expect("the first plan is active")
        .to_owned();

    let second = run(
        "/plan --new 为 notes.txt 增加一个统计单词数的函数，并说明哪些核心逻辑需要用户自己实现。",
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second.status.success(),
        "/plan --new failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let index: serde_json::Value =
        serde_json::from_slice(&fs::read(&index_path).expect("index is readable"))
            .expect("index is JSON");
    let new_id = index["active"]
        .as_str()
        .expect("the second plan is active")
        .to_owned();
    assert_ne!(new_id, old_id);
    let old_state: serde_json::Value = serde_json::from_slice(
        &fs::read(
            workspace
                .join(".scratch/workflows")
                .join(&old_id)
                .join("state.json"),
        )
        .expect("old state is readable"),
    )
    .expect("old state is JSON");
    assert_eq!(
        old_state["status"].as_str(),
        Some("paused"),
        "the previous workflow is paused, not ended"
    );

    let third = run("/plan 再增加一个别的函数。");
    let stdout = String::from_utf8_lossy(&third.stdout);
    let stderr = String::from_utf8_lossy(&third.stderr);
    assert!(
        !third.status.success(),
        "a third /plan must be rejected\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("still active") && stderr.contains("/plan --new"),
        "the rejection names the active workflow and the flag\nstderr:\n{stderr}"
    );

    println!("stdout:\n{stdout}");
    println!("stderr:\n{stderr}");
    let _ = fs::remove_dir_all(&workspace);
}
