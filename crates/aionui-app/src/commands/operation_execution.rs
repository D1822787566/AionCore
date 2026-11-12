//! Per-command execution facts, including failures after a successful write.
use serde_json::{Value, json};
use std::cell::RefCell;

#[derive(Default)]
pub(super) struct ExecutionTrace {
    sent: bool,
    write_succeeded: bool,
}
tokio::task_local! { pub(super) static TRACE: RefCell<ExecutionTrace>; }

pub(super) fn request_started() {
    let _ = TRACE.try_with(|trace| trace.borrow_mut().sent = true);
}
pub(super) fn write_succeeded() {
    let _ = TRACE.try_with(|trace| trace.borrow_mut().write_succeeded = true);
}
pub(super) fn failure() -> Value {
    TRACE
        .try_with(|trace| {
            let trace = trace.borrow();
            if trace.write_succeeded {
                json!({"status":"succeeded","stage":"readback_or_delivery","retry_write":false})
            } else if trace.sent {
                json!({"status":"unknown","stage":"backend_request","retry_write":false})
            } else {
                json!({"status":"not_executed","stage":"preflight"})
            }
        })
        .unwrap_or(json!({"status":"unknown","stage":"untracked"}))
}

pub(super) fn local_failure<T: serde::Serialize>(envelope: T, code: &str) -> Value {
    let mut value = serde_json::to_value(envelope).expect("CLI failure envelope");
    let local = [
        "_ENV_MISSING",
        "_HEADER_INVALID",
        "_STDIN_READ_FAILED",
        "_STDIN_JSON_INVALID",
        "_SCHEMA_VALIDATION_FAILED",
        "_UNKNOWN_COMMAND",
    ]
    .iter()
    .any(|suffix| code.ends_with(suffix));
    value["execution"] = json!({"status":if local {"not_executed"} else {"unknown"},"stage":if local {"preflight"} else {"backend_request"}});
    value
}
