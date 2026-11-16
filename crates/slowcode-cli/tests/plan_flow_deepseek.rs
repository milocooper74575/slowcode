//! Real DeepSeek chain for the Plan command flow. Ignored by default:
//!
//! ```bash
//! cargo test -p slowcode-cli --test plan_flow_deepseek -- --ignored --nocapture
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
        .expect("clock is after epoch")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("slowcode-real-flow-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

fn write_plan(workspace: &Path) -> PathBuf {
    let directory = workspace.join(".scratch/plans");
    fs::create_dir_all(&directory).expect("plan directory is created");
    let path = directory.join("1000-plan.md");
    let artifact = "\
---
kind: plan
created_at_unix: 1000
model: fixture
workspace: test
status: paused
phases: Explore, Navigate
validation: ok
---

# Plan: 为 notes.txt 增加行数统计

## Agent output

## 证据包
- notes.txt 存在且有两行。

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取 notes.txt 并确认行数 | 已知域 | notes.txt 存在 | 可执行 | 自主执行 | 审查者 | read notes.txt | 输出 2 |
| 选择行数口径 | 半已知域 | 仓库没有行数规范，需要用户决定 | 待决策 | 给出选项 | 决策者 | 决策后实现 | 用户确认 |
| 实现核心计数函数 | 未知域 | 核心实现必须由用户完成 | 待用户实现 | 搭建脚手架 | 创造者 | 空函数体 | 通过测试 |

## 半已知域决策
- 选择行数口径：A 按换行符计数，B 按 splitlines 计数。

## 未知域暂停
- 用哪种语言与文件路径？
- 核心计数函数由用户实现。
";
    fs::write(&path, artifact).expect("plan artifact is written");
    path
}

fn run_cli(root: &Path, workspace: &Path, plan: &Path, args: &[&str], label: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .current_dir(root)
        .arg("--workspace")
        .arg(workspace)
        .arg("--plan")
        .arg(plan)
        .arg("--model")
        .arg(std::env::var("SLOWCODE_E2E_MODEL").unwrap_or_else(|_| "deepseek-flash".to_owned()))
        .arg("--max-tokens")
        .arg("32768")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{label} failed to run: {error}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    println!("=== {label} stdout ===\n{stdout}");
    println!("=== {label} stderr ===\n{stderr}");
    stdout.into_owned()
}

#[test]
#[ignore = "requires DEEPSEEK_API_KEY and makes four real API calls"]
fn answer_scaffold_execute_distill_real_chain() {
    let root = repo_root();
    let _ = dotenvy::from_path(root.join(".env"));
    assert!(
        std::env::var_os("DEEPSEEK_API_KEY").is_some(),
        "DEEPSEEK_API_KEY is missing; put it in {} or export it",
        root.join(".env").display()
    );

    let workspace = temp_workspace();
    fs::write(workspace.join("notes.txt"), "alpha\nbeta\n").expect("notes.txt is written");
    let plan = write_plan(&workspace);

    run_cli(
        &root,
        &workspace,
        &plan,
        &[
            "/answer",
            "用 Python，文件写到 count_lines.py；行数按 splitlines 口径；空文件返回 0；末行无换行计 1；文件不存在时抛 FileNotFoundError。核心计数函数由我自己实现，不要代写。",
        ],
        "answer",
    );

    run_cli(&root, &workspace, &plan, &["/scaffold"], "scaffold");

    let scaffold_path = workspace.join("count_lines.py");
    assert!(
        scaffold_path.exists(),
        "scaffold did not create count_lines.py; files: {:?}",
        fs::read_dir(&workspace)
            .map(|entries| entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .collect::<Vec<_>>())
            .unwrap_or_default()
    );
    let scaffold = fs::read_to_string(&scaffold_path).expect("scaffold is readable");
    let lower = scaffold.to_lowercase();
    assert!(
        lower.contains("todo") || lower.contains("notimplemented") || lower.contains("pass"),
        "scaffold does not look like a blank core:\n{scaffold}"
    );

    run_cli(&root, &workspace, &plan, &["/execute"], "execute");
    run_cli(
        &root,
        &workspace,
        &plan,
        &[
            "/distill",
            "我按 splitlines 实现了计数，空文件返回 0，末行无换行计 1。",
        ],
        "distill",
    );

    let artifact = fs::read_to_string(&plan).expect("artifact is readable");
    assert!(artifact.contains("## 决策记录"), "{artifact}");
    assert!(artifact.contains("## Scaffold"), "{artifact}");
    assert!(artifact.contains("## Execution"), "{artifact}");
    assert!(artifact.contains("## Distill"), "{artifact}");
    assert!(artifact.contains("status: distilled"), "{artifact}");

    let _ = fs::remove_dir_all(&workspace);
}
