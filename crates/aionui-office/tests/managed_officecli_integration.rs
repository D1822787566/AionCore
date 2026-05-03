//! A separate test process isolates environment changes from the rest of the suite.
use std::sync::{Arc, Mutex};

use aionui_api_types::WebSocketMessage;
use aionui_office::{DefaultProcessSpawner, DocType, OfficeError, OfficecliWatchManager, ProcessSpawner};
use aionui_realtime::EventBroadcaster;

#[derive(Default)]
struct Events(Mutex<Vec<String>>);

impl EventBroadcaster for Events {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.0
            .lock()
            .unwrap()
            .push(event.data["state"].as_str().unwrap().into());
    }
}

#[tokio::test]
async fn managed_missing_binary_never_installs_or_checks_online() {
    let tmp = tempfile::tempdir().unwrap();
    // This binary contains only this test and creates no other threads before setting env.
    unsafe {
        std::env::set_var("AIONUI_OFFICECLI_MANAGED", "1");
        std::env::set_var("AIONUI_OFFICECLI_PATH", tmp.path().join("missing-officecli.exe"));
    }
    let spawner = Arc::new(DefaultProcessSpawner::new(tmp.path().to_owned()));
    assert!(!spawner.is_officecli_installed().await);
    assert!(matches!(
        spawner.install_officecli().await,
        Err(OfficeError::OfficecliNotFound)
    ));
    // Returns before path lookup/network I/O, including when the bundled file is missing.
    spawner.check_update(DocType::Ppt).await.unwrap();

    let file = tmp.path().join("sample.pptx");
    std::fs::write(&file, b"fixture").unwrap();
    let events = Arc::new(Events::default());
    let manager = OfficecliWatchManager::new(spawner, events.clone());
    assert!(matches!(
        manager.start(file.to_str().unwrap(), DocType::Ppt).await,
        Err(OfficeError::OfficecliNotFound)
    ));
    assert_eq!(*events.0.lock().unwrap(), vec!["starting", "error"]);
}
