//! Adapter status → the built-in provider specification.
//!
//! Pure mapping, no I/O: the service decides *when* to publish, this module
//! decides *what* the published row contains. Keeping it pure is what makes the
//! "which models, which settings, which URL" rules testable without an adapter.

use std::collections::BTreeMap;

use aionui_api_types::{BaitongAdapterState, BaitongStatusResponse, ModelImageInputCapability, ModelSettings};

/// Loopback address aionrs uses to reach the adapter.
///
/// Always `127.0.0.1`: `lan` mode binds `0.0.0.0`, which is a listen address, not
/// a connect address. AionCore and aionrs both run on this machine, so the
/// provider never needs the LAN name.
const LOOPBACK: &str = "127.0.0.1";

/// The vision MCP endpoint the adapter exposes next to its OpenAI-compatible API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionMcp {
    pub url: String,
    pub enabled: bool,
}

/// Everything AionCore owns on the built-in provider row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSpec {
    pub base_url: String,
    pub models: Vec<String>,
    pub model_settings: BTreeMap<String, ModelSettings>,
    pub vision_mcp: Option<VisionMcp>,
}

/// Build the managed row specification, or `None` when the adapter cannot serve
/// models right now.
///
/// `None` means "publish nothing": a stopped adapter or an empty catalog must not
/// rewrite the row to an empty model list, because that would leave the user's
/// provider list showing a provider that silently serves nothing.
pub fn spec_from_status(status: &BaitongStatusResponse) -> Option<ManagedSpec> {
    if status.adapter.state != BaitongAdapterState::Running {
        return None;
    }
    if status.models.items.is_empty() {
        return None;
    }

    let port = status.adapter.port;
    // Every catalog model is marked image-capable. The adapter does not call the
    // upstream model with image blocks: it detects `image_url` parts, uploads
    // them, and exposes `look_at_image` over its own MCP endpoint. aionrs needs
    // `image_input = supported` to stop stripping attachments and to keep the
    // `ViewImage` tool enabled, so the capability is a property of the adapter,
    // not of the individual upstream model.
    let model_settings = status
        .models
        .items
        .iter()
        .map(|model| {
            (
                model.clone(),
                ModelSettings {
                    image_input: Some(ModelImageInputCapability::Supported),
                    ..ModelSettings::default()
                },
            )
        })
        .collect();

    Some(ManagedSpec {
        base_url: format!("http://{LOOPBACK}:{port}/v1"),
        models: status.models.items.clone(),
        model_settings,
        vision_mcp: Some(VisionMcp {
            url: format!("http://{LOOPBACK}:{port}/mcp"),
            enabled: status.models.vision_mcp_enabled,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{
        BaitongAdapterStatus, BaitongConfigSnapshot, BaitongCookieState, BaitongCredentialState,
        BaitongEnterpriseState, BaitongEnterpriseStatus, BaitongModelCatalog, BaitongSessionState,
        BaitongSessionStatus, BaitongVisionCatalog,
    };

    fn status_with(state: BaitongAdapterState, models: Vec<&str>, vision_mcp_enabled: bool) -> BaitongStatusResponse {
        BaitongStatusResponse {
            adapter: BaitongAdapterStatus {
                state,
                host: "0.0.0.0".to_string(),
                port: 8102,
                bind_mode: "lan".to_string(),
                display_host: "192.168.1.20".to_string(),
                managed: true,
                last_error: None,
            },
            session: BaitongSessionStatus {
                state: BaitongSessionState::Authenticated,
                account: Some("u@b.com".to_string()),
                cookie_state: BaitongCookieState::Valid,
                credential_state: BaitongCredentialState::Ready,
                last_refresh_at: None,
            },
            enterprise: BaitongEnterpriseStatus {
                state: BaitongEnterpriseState::Deferred,
            },
            models: BaitongModelCatalog {
                items: models.into_iter().map(str::to_owned).collect(),
                default: None,
                thinking: Vec::new(),
                vision: BaitongVisionCatalog {
                    items: Vec::new(),
                    selected: None,
                },
                vision_mcp_enabled,
            },
            config: BaitongConfigSnapshot {
                version: 0,
                last_applied_at: None,
                port: 8102,
                bind_mode: "lan".to_string(),
                lan_host: String::new(),
                log_call: false,
                log_interaction: false,
            },
        }
    }

    #[test]
    fn running_adapter_publishes_every_model_as_image_capable() {
        let status = status_with(
            BaitongAdapterState::Running,
            vec!["DeepSeek-V4-Flash", "Qwen3.5-35B-A3B"],
            true,
        );

        let spec = spec_from_status(&status).expect("running adapter with models must publish");

        assert_eq!(spec.models, vec!["DeepSeek-V4-Flash", "Qwen3.5-35B-A3B"]);
        assert_eq!(spec.model_settings.len(), 2);
        for model in &spec.models {
            assert_eq!(
                spec.model_settings[model].image_input,
                Some(ModelImageInputCapability::Supported),
                "model {model} must be image capable"
            );
        }
    }

    #[test]
    fn base_url_is_loopback_v1_even_when_the_adapter_listens_on_lan() {
        // The adapter reports `0.0.0.0` / a LAN display host in lan mode, but
        // aionrs must reach it over loopback: a listen address is not routable.
        let status = status_with(BaitongAdapterState::Running, vec!["m"], true);

        let spec = spec_from_status(&status).expect("spec");

        assert_eq!(spec.base_url, "http://127.0.0.1:8102/v1");
        assert_eq!(
            spec.vision_mcp.as_ref().map(|v| v.url.as_str()),
            Some("http://127.0.0.1:8102/mcp")
        );
    }

    #[test]
    fn stopped_adapter_publishes_nothing() {
        let status = status_with(BaitongAdapterState::Stopped, vec!["m"], true);
        assert!(spec_from_status(&status).is_none());
    }

    #[test]
    fn empty_catalog_publishes_nothing_so_the_row_is_not_wiped() {
        let status = status_with(BaitongAdapterState::Running, Vec::new(), true);
        assert!(spec_from_status(&status).is_none());
    }

    #[test]
    fn vision_mcp_switch_is_carried_through() {
        let on = status_with(BaitongAdapterState::Running, vec!["m"], true);
        let off = status_with(BaitongAdapterState::Running, vec!["m"], false);

        assert!(spec_from_status(&on).unwrap().vision_mcp.unwrap().enabled);
        assert!(!spec_from_status(&off).unwrap().vision_mcp.unwrap().enabled);
    }
}
