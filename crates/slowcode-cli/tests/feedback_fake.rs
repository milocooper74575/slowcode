//! The feedback bundle script turns one session journal into an issue draft.

#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const SESSION_ID: &str = "session-33333333-3333-4333-8333-333333333333";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/slowcode-cli is two levels below the repo root")
        .to_owned()
}

fn temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-feedback-{label}-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp directory is created");
    path
}

fn journal() -> String {
    format!(
        concat!(
            "{{\"type\":\"session\",\"id\":\"{id}\",\"createdAt\":1792466172940,",
            "\"cwd\":\"/tmp/example-project\"}}\n",
            "{{\"type\":\"user/message\",\"seq\":0,\"data\":{{\"content\":[{{\"type\":\"text\",",
            "\"text\":\"explain the failure\"}}]}}}}\n",
            "{{\"type\":\"assistant/message\",\"seq\":1,\"data\":{{\"message\":{{\"content\":[",
            "{{\"type\":\"text\",\"text\":\"the answer\"}}]}},\"usage\":{{",
            "\"inputTokens\":10,\"outputTokens\":2,\"cacheReadTokens\":5,",
            "\"reasoningTokens\":0}},\"turn\":1}}}}\n",
            "{{\"type\":\"turn/end\",\"seq\":2,\"data\":{{\"reason\":{{\"kind\":\"completed\"}},",
            "\"turn\":1}}}}\n"
        ),
        id = SESSION_ID
    )
}

#[test]
fn feedback_script_writes_issue_and_metadata() {
    let base = temp_dir("bundle");
    let store = base.join("store");
    fs::create_dir_all(&store).expect("store is created");
    fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).expect("store is private");
    let journal_path = store.join(format!("{SESSION_ID}.jsonl"));
    fs::write(&journal_path, journal()).expect("journal is written");
    fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600))
        .expect("journal is private");
    let out = base.join("out");
    let plan = base.join("plan.md");
    fs::write(&plan, "# plan\n").expect("plan is written");

    let output = Command::new("bash")
        .arg(repo_root().join("scripts/feedback.sh"))
        .arg(SESSION_ID)
        .arg("--out")
        .arg(&out)
        .arg("--plan")
        .arg(&plan)
        .arg("--title")
        .arg("a real failure")
        .arg("--note")
        .arg("expected success")
        .env("DSH_SESSION_ROOT", &store)
        .env("SLOWCODE_BIN", env!("CARGO_BIN_EXE_slowcode"))
        .output()
        .expect("feedback script runs");
    assert!(
        output.status.success(),
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let issue = fs::read_to_string(out.join("issue.md")).expect("issue is readable");
    assert!(issue.contains("# [feedback] a real failure"), "{issue}");
    assert!(issue.contains(SESSION_ID), "{issue}");
    assert!(
        issue.contains("workspace-name: `example-project`"),
        "{issue}"
    );
    assert!(issue.contains("turns: 1 (completed x1)"), "{issue}");
    assert!(
        issue.contains("input 10, output 2, cache read 5"),
        "{issue}"
    );
    assert!(issue.contains("expected success"), "{issue}");
    assert!(!issue.contains("/tmp/example-project"), "{issue}");
    assert!(issue.contains("`plan.md`"), "{issue}");

    let metadata = fs::read_to_string(out.join("metadata.json")).expect("metadata is readable");
    assert!(
        metadata.contains("\"traceId\": \"session-33333333"),
        "{metadata}"
    );
    assert!(out.join("session.jsonl").is_file());
    assert!(out.join("plan.md").is_file());
    let manifest = fs::read_to_string(out.join("manifest.json")).expect("manifest is readable");
    assert!(manifest.contains("\"complete\": true"), "{manifest}");
    assert_eq!(
        fs::metadata(out.join("session.jsonl"))
            .expect("journal metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn feedback_script_fails_the_hard_gate_for_a_seq_gap() {
    let base = temp_dir("gap");
    let store = base.join("store");
    fs::create_dir_all(&store).expect("store is created");
    fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).expect("store is private");
    let journal_path = store.join(format!("{SESSION_ID}.jsonl"));
    let journal = journal().replace("\"seq\":1", "\"seq\":7");
    fs::write(&journal_path, journal).expect("journal is written");
    fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600))
        .expect("journal is private");
    let out = base.join("out");

    let output = Command::new("bash")
        .arg(repo_root().join("scripts/feedback.sh"))
        .arg(SESSION_ID)
        .arg("--out")
        .arg(&out)
        .env("DSH_SESSION_ROOT", &store)
        .env("SLOWCODE_BIN", env!("CARGO_BIN_EXE_slowcode"))
        .output()
        .expect("feedback script runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("INCOMPLETE"), "{stderr}");

    let issue = fs::read_to_string(out.join("issue.md")).expect("issue is readable");
    assert!(issue.contains("INCOMPLETE BUNDLE"), "{issue}");
    assert!(issue.contains("complete: `false`"), "{issue}");
    let manifest = fs::read_to_string(out.join("manifest.json")).expect("manifest is readable");
    assert!(manifest.contains("\"complete\": false"), "{manifest}");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn feedback_script_rejects_an_invalid_session_id() {
    let output = Command::new("bash")
        .arg(repo_root().join("scripts/feedback.sh"))
        .arg("not-a-session")
        .env("SLOWCODE_BIN", env!("CARGO_BIN_EXE_slowcode"))
        .output()
        .expect("feedback script runs");
    assert!(!output.status.success());
}
