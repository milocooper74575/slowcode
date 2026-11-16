//! Public read-only session catalog seam.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use slowcode_kernel::session::{SessionStore, new_session_id};

fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "slowcode-session-catalog-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).expect("temp dir is created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("temp dir is private");
    }
    path
}

#[tokio::test]
async fn list_for_workspace_returns_only_that_workspace() {
    let root = temp_dir("store");
    let workspace = temp_dir("workspace");
    let other = temp_dir("other");
    // SAFETY: this integration test binary owns its process environment.
    unsafe { std::env::set_var("DSH_SESSION_ROOT", &root) };

    let store = SessionStore::open_default().expect("store opens");
    let id = new_session_id();
    let mut session = store
        .open_new_session(id.clone(), &workspace)
        .await
        .expect("session is created");
    session.shutdown().await.expect("session flushes");

    let listed = store
        .list_for_workspace(Some(&workspace))
        .expect("session list reads");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id(), &id);
    assert_eq!(listed[0].workspace(), workspace.to_string_lossy());

    let other_listed = store
        .list_for_workspace(Some(&other))
        .expect("other workspace list reads");
    assert!(other_listed.is_empty());

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(workspace);
    let _ = fs::remove_dir_all(other);
}
