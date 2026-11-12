//! Agent-readable config contract shared with the host and SDK injection.
use serde_json::Value;
pub(crate) fn data() -> Value {
    aionui_api_types::config_operation_capabilities()
}
