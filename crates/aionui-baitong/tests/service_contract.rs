//! Service-level contract tests: what `BaitongService` reports when the adapter
//! cannot start, and the ordering guarantees of config version checks.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{BaitongAdapterState, BaitongConfigUpdate};
use aionui_baitong::{AdapterLaunch, AdapterSupervisor, BaitongService};

fn service_with_broken_launch() -> (BaitongService, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let launch = AdapterLaunch {
        program: PathBuf::from("definitely-not-a-real-adapter-binary"),
        args: Vec::new(),
        cwd: None,
    };
    let supervisor = Arc::new(AdapterSupervisor::new(launch, dir.path().to_path_buf()));
    (BaitongService::new(supervisor, dir.path().to_path_buf()), dir)
}

#[tokio::test]
async fn status_reports_unavailable_instead_of_failing_when_spawn_fails() {
    let (service, _dir) = service_with_broken_launch();

    let status = service.status().await.expect("status must not be an error");

    assert_eq!(status.adapter.state, BaitongAdapterState::Unavailable);
    assert!(status.adapter.managed);
    assert!(
        status
            .adapter
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("failed to spawn"),
        "unexpected last_error: {:?}",
        status.adapter.last_error
    );
}

#[tokio::test]
async fn status_restores_only_once_so_a_poll_does_not_respawn() {
    let (service, _dir) = service_with_broken_launch();

    let first = service.status().await.expect("first status");
    let second = service.status().await.expect("second status");

    assert_eq!(first.adapter.state, BaitongAdapterState::Unavailable);
    // No second spawn attempt: the failure is remembered for this session and
    // the UI gets the plain "stopped" view plus its start button.
    assert_eq!(second.adapter.state, BaitongAdapterState::Stopped);
    assert!(second.adapter.last_error.is_none());
}

#[tokio::test]
async fn config_version_conflict_is_rejected_before_touching_the_adapter() {
    let (service, _dir) = service_with_broken_launch();

    let error = service
        .update_config(BaitongConfigUpdate {
            port: Some(8200),
            expected_version: Some(7),
            ..BaitongConfigUpdate::default()
        })
        .await
        .expect_err("stale expected_version must be rejected");

    // The adapter binary does not exist; reaching it would have produced
    // BAITONG_ADAPTER_ERROR instead.
    assert_eq!(error.code(), "BAITONG_CONFIG_CONFLICT");
}

#[tokio::test]
async fn logout_clears_credentials_and_reports_a_stopped_view() {
    let (service, _dir) = service_with_broken_launch();

    let status = service.logout().await.expect("logout must not fail");

    assert_eq!(status.adapter.state, BaitongAdapterState::Stopped);
    assert!(status.session.account.is_none());
    assert_eq!(status.config.version, 0);
}

#[tokio::test]
async fn login_rejects_empty_fields_without_starting_the_adapter() {
    let (service, _dir) = service_with_broken_launch();

    let error = service.login("   ", "secret").await.unwrap_err();
    assert_eq!(error.code(), "BAITONG_BAD_REQUEST");

    let error = service.login("someone", "").await.unwrap_err();
    assert_eq!(error.code(), "BAITONG_BAD_REQUEST");
}
