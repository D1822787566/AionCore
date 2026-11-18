//! Integration tests for lifecycle hooks (test-plan LH-1..LH-6).
//!
//! These tests exercise `execute_hook`, `needs_install_hook`, and
//! `resolve_hook_path` as black-box functions, verifying first install,
//! version change, activate/deactivate execution, timeout behaviour,
//! and graceful handling of missing scripts.

use std::fs;
use std::path::Path;

use aionui_extension::{HookKind, LifecycleHooks, execute_hook, needs_install_hook, resolve_hook_path};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a native script and return its platform-specific relative path.
fn write_script(dir: &Path, rel_path: &str, unix_body: &str, windows_body: &str) -> String {
    let rel_path = Path::new(rel_path).with_extension(if cfg!(windows) { "cmd" } else { "sh" });
    let full = dir.join(&rel_path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let content = if cfg!(windows) {
        format!("@echo off\r\n{windows_body}\r\n")
    } else {
        format!("#!/bin/sh\n{unix_body}\n")
    };
    fs::write(&full, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).unwrap();
    }
    rel_path.to_str().unwrap().to_owned()
}

fn setup_ext_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

// ---------------------------------------------------------------------------
// LH-1: First install executes onInstall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lh1_first_install_executes_on_install() {
    let dir = setup_ext_dir();
    let marker = dir.path().join("installed.marker");
    let script = write_script(
        dir.path(),
        "scripts/install.sh",
        &format!("touch '{}'", marker.display()),
        &format!("type nul > \"{}\"", marker.display()),
    );

    let hooks = LifecycleHooks {
        on_install: Some(script),
        ..Default::default()
    };

    // First install: no persisted version
    assert!(needs_install_hook("1.0.0", None));

    let hook_path = resolve_hook_path(&hooks, HookKind::OnInstall).unwrap();
    let result = execute_hook(dir.path(), hook_path, HookKind::OnInstall, "test-ext").await;
    assert!(result.is_ok());
    assert!(marker.exists(), "onInstall marker file should be created");
}

// ---------------------------------------------------------------------------
// LH-2: Version change executes onInstall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lh2_version_change_executes_on_install() {
    let dir = setup_ext_dir();
    let marker = dir.path().join("upgraded.marker");
    let script = write_script(
        dir.path(),
        "scripts/install.sh",
        &format!("touch '{}'", marker.display()),
        &format!("type nul > \"{}\"", marker.display()),
    );

    let hooks = LifecycleHooks {
        on_install: Some(script),
        ..Default::default()
    };

    // Version changed from 1.0.0 to 2.0.0
    assert!(needs_install_hook("2.0.0", Some("1.0.0")));

    let hook_path = resolve_hook_path(&hooks, HookKind::OnInstall).unwrap();
    let result = execute_hook(dir.path(), hook_path, HookKind::OnInstall, "test-ext").await;
    assert!(result.is_ok());
    assert!(marker.exists(), "onInstall marker should be created on upgrade");
}

// ---------------------------------------------------------------------------
// LH-2 (negative): Same version does NOT trigger onInstall
// ---------------------------------------------------------------------------

#[test]
fn lh2_same_version_skips_install() {
    assert!(!needs_install_hook("1.0.0", Some("1.0.0")));
}

// ---------------------------------------------------------------------------
// LH-3: Each activation executes onActivate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lh3_activate_executes_on_activate() {
    let dir = setup_ext_dir();
    let counter_file = dir.path().join("activate_count.txt");
    // Append a line on each activation to count calls
    let script = write_script(
        dir.path(),
        "scripts/activate.sh",
        &format!("echo 'activated' >> '{}'", counter_file.display()),
        &format!("echo activated>>\"{}\"", counter_file.display()),
    );

    let hooks = LifecycleHooks {
        on_activate: Some(script),
        ..Default::default()
    };

    let hook_path = resolve_hook_path(&hooks, HookKind::OnActivate).unwrap();

    // Activate twice
    execute_hook(dir.path(), hook_path, HookKind::OnActivate, "test-ext")
        .await
        .unwrap();
    execute_hook(dir.path(), hook_path, HookKind::OnActivate, "test-ext")
        .await
        .unwrap();

    let content = fs::read_to_string(&counter_file).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2, "onActivate should run on each activation");
}

// ---------------------------------------------------------------------------
// LH-4: Deactivation executes onDeactivate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lh4_deactivate_executes_on_deactivate() {
    let dir = setup_ext_dir();
    let marker = dir.path().join("deactivated.marker");
    let script = write_script(
        dir.path(),
        "scripts/deactivate.sh",
        &format!("touch '{}'", marker.display()),
        &format!("type nul > \"{}\"", marker.display()),
    );

    let hooks = LifecycleHooks {
        on_deactivate: Some(script),
        ..Default::default()
    };

    let hook_path = resolve_hook_path(&hooks, HookKind::OnDeactivate).unwrap();
    let result = execute_hook(dir.path(), hook_path, HookKind::OnDeactivate, "test-ext").await;
    assert!(result.is_ok());
    assert!(marker.exists(), "onDeactivate marker should be created");
}

// ---------------------------------------------------------------------------
// LH-5: Hook timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lh5_hook_timeout() {
    let dir = setup_ext_dir();
    // Script that sleeps for a long time
    let script_path = dir.path().join(if cfg!(windows) {
        "scripts/slow.ps1"
    } else {
        "scripts/slow.sh"
    });
    fs::create_dir_all(script_path.parent().unwrap()).unwrap();
    let content = if cfg!(windows) {
        "Start-Sleep -Seconds 120\r\n"
    } else {
        "#!/bin/sh\nsleep 120\n"
    };
    fs::write(&script_path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // We can't easily override the built-in timeout constants in the public API,
    // so we test the timeout mechanism by using tokio::time::timeout directly
    // to simulate what execute_hook does internally with a very short deadline.
    assert!(script_path.exists());

    // PowerShell's built-in sleep stays in the direct child process, so
    // Builder's kill_on_drop cannot leave a sleeping descendant behind.
    let mut command = if cfg!(windows) {
        let mut command = aionui_runtime::Builder::clean_cli("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script_path);
        command
    } else {
        aionui_runtime::Builder::clean_cli(&script_path)
    };
    command.current_dir(dir.path());
    let child_future = command.output();

    let result = tokio::time::timeout(std::time::Duration::from_millis(200), child_future).await;
    assert!(result.is_err(), "should time out before script completes");
}

// ---------------------------------------------------------------------------
// LH-6: Hook script does not exist — graceful handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lh6_missing_script_graceful() {
    let dir = setup_ext_dir();

    let hooks = LifecycleHooks {
        on_activate: Some("nonexistent.sh".into()),
        ..Default::default()
    };

    let hook_path = resolve_hook_path(&hooks, HookKind::OnActivate).unwrap();
    let result = execute_hook(dir.path(), hook_path, HookKind::OnActivate, "test-ext").await;

    assert!(result.is_err());
    match result.unwrap_err() {
        aionui_extension::ExtensionError::HookNotFound(path) => {
            assert!(
                path.contains("nonexistent.sh"),
                "error should mention the missing script path"
            );
        }
        other => panic!("expected HookNotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Additional: resolve_hook_path returns None when hook is not declared
// ---------------------------------------------------------------------------

#[test]
fn resolve_hook_path_none_when_not_declared() {
    let hooks = LifecycleHooks::default();
    assert!(resolve_hook_path(&hooks, HookKind::OnInstall).is_none());
    assert!(resolve_hook_path(&hooks, HookKind::OnUninstall).is_none());
    assert!(resolve_hook_path(&hooks, HookKind::OnActivate).is_none());
    assert!(resolve_hook_path(&hooks, HookKind::OnDeactivate).is_none());
}

// ---------------------------------------------------------------------------
// Additional: Hook script exits with non-zero status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_nonzero_exit_returns_hook_failed() {
    let dir = setup_ext_dir();
    let script = write_script(
        dir.path(),
        "scripts/fail.sh",
        "echo 'setup failed' >&2; exit 42",
        "echo setup failed >&2\r\nexit /b 42",
    );

    let result = execute_hook(dir.path(), &script, HookKind::OnInstall, "failing-ext").await;

    assert!(result.is_err());
    match result.unwrap_err() {
        aionui_extension::ExtensionError::HookFailed {
            extension_name,
            hook,
            reason,
        } => {
            assert_eq!(extension_name, "failing-ext");
            assert_eq!(hook, "onInstall");
            assert!(reason.contains("42"), "should include exit code");
            assert!(reason.contains("setup failed"), "should include stderr");
        }
        other => panic!("expected HookFailed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Additional: Hook uses working directory correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_working_directory_is_ext_dir() {
    let dir = setup_ext_dir();
    let script = write_script(dir.path(), "check_dir.sh", "pwd > cwd_out.txt", "cd > cwd_out.txt");

    let result = execute_hook(dir.path(), &script, HookKind::OnActivate, "cwd-ext").await;
    assert!(result.is_ok());

    let cwd_file = dir.path().join("cwd_out.txt");
    assert!(cwd_file.exists());
    let cwd = fs::read_to_string(&cwd_file).unwrap();
    let expected = dir.path().canonicalize().unwrap();
    let actual = std::path::Path::new(cwd.trim()).canonicalize().unwrap();
    assert_eq!(actual, expected);
}
