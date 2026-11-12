//! Public stdin contracts. Extra fields stay open wherever the CLI/API ignores them.
use serde_json::{Value, json};

fn text() -> Value {
    json!({"type":"string"})
}
fn id() -> Value {
    json!({"type":"string","minLength":1})
}
fn boolean() -> Value {
    json!({"type":"boolean"})
}
fn integer() -> Value {
    json!({"type":"integer"})
}
fn array(items: Value) -> Value {
    json!({"type":"array","items":items})
}
fn dictionary(values: Value) -> Value {
    json!({"type":"object","additionalProperties":values})
}
fn nullable(schema: Value) -> Value {
    json!({"anyOf":[schema,{"type":"null"}]})
}
fn object(required: &[&str], fields: &[(&str, Value)]) -> Value {
    json!({"type":"object","required":required,"properties":fields.iter().map(|(k,v)|(k.to_string(),v.clone())).collect::<serde_json::Map<_,_>>()})
}

fn assistant_defaults() -> Value {
    let scalar = object(&["mode"], &[("mode", text()), ("value", nullable(text()))]);
    let list = object(&["mode"], &[("mode", text()), ("value", array(text()))]);
    object(
        &[],
        &[
            ("model", nullable(scalar.clone())),
            ("permission", nullable(scalar.clone())),
            ("thought_level", nullable(scalar)),
            ("skills", nullable(list.clone())),
            ("mcps", nullable(list)),
        ],
    )
}
fn bedrock() -> Value {
    object(
        &["auth_method", "region"],
        &[
            ("auth_method", json!({"enum":["accessKey","profile"]})),
            ("region", text()),
            ("access_key_id", nullable(text())),
            ("secret_access_key", nullable(text())),
            ("profile", nullable(text())),
        ],
    )
}
fn capability() -> Value {
    object(
        &["type"],
        &[
            (
                "type",
                json!({"enum":["text","vision","function_calling","image_generation","web_search","reasoning","embedding","rerank","excludeFromPrimary"]}),
            ),
            ("is_user_selected", nullable(boolean())),
        ],
    )
}
fn model_health() -> Value {
    object(
        &["status"],
        &[
            ("status", json!({"enum":["unknown","healthy","unhealthy"]})),
            ("last_check", nullable(integer())),
            ("latency", nullable(integer())),
            ("error", nullable(text())),
        ],
    )
}
fn model_settings() -> Value {
    object(
        &[],
        &[
            ("image_input", nullable(json!({"enum":["supported","unsupported"]}))),
            (
                "openai_api_mode",
                nullable(json!({"enum":["chat_completions","responses"]})),
            ),
        ],
    )
}
fn advanced_agent() -> Value {
    let policy = object(
        &[],
        &[
            ("supports_side_question", boolean()),
            ("self_identity_sticky", boolean()),
            ("session_load_via_meta_field", boolean()),
            ("supports_team", boolean()),
        ],
    );
    object(
        &[],
        &[
            ("yolo_id", nullable(text())),
            ("native_skills_dirs", nullable(array(text()))),
            ("behavior_policy", nullable(policy)),
            ("description", nullable(text())),
        ],
    )
}
fn cron_agent() -> Value {
    let mut schema = object(
        &["name"],
        &[
            ("name", text()),
            ("cli_path", nullable(text())),
            ("assistant_id", nullable(text())),
            ("mode", nullable(text())),
            ("model_id", nullable(text())),
            (
                "model",
                nullable(object(
                    &["provider_id", "model"],
                    &[
                        ("provider_id", text()),
                        ("model", text()),
                        ("use_model", nullable(text())),
                    ],
                )),
            ),
            ("config_options", nullable(dictionary(text()))),
            ("workspace", nullable(text())),
        ],
    );
    schema["additionalProperties"] = json!(false);
    schema
}

fn assistant(required: &[&str]) -> Value {
    object(
        required,
        &[
            ("assistant_id", id()),
            ("locale", nullable(text())),
            ("id", nullable(text())),
            ("name", nullable(text())),
            ("description", nullable(text())),
            ("avatar", nullable(text())),
            ("agent_id", nullable(text())),
            ("enabled_skills", nullable(array(text()))),
            ("custom_skill_names", nullable(array(text()))),
            ("disabled_builtin_skills", nullable(array(text()))),
            ("prompts", nullable(array(text()))),
            ("models", nullable(array(text()))),
            ("recommended_prompts", nullable(array(text()))),
            ("name_i18n", nullable(dictionary(text()))),
            ("description_i18n", nullable(dictionary(text()))),
            ("prompts_i18n", nullable(dictionary(array(text())))),
            ("recommended_prompts_i18n", nullable(dictionary(array(text())))),
            ("defaults", nullable(assistant_defaults())),
        ],
    )
}

fn provider(required: &[&str]) -> Value {
    object(
        required,
        &[
            ("provider_id", id()),
            ("id", nullable(text())),
            ("name", nullable(text())),
            ("platform", nullable(text())),
            ("base_url", nullable(text())),
            ("api_key", nullable(text())),
            ("models", nullable(array(text()))),
            ("enabled", nullable(boolean())),
            ("context_limit", nullable(integer())),
            ("capabilities", nullable(array(capability()))),
            ("model_protocols", nullable(dictionary(text()))),
            ("model_enabled", nullable(dictionary(boolean()))),
            ("model_health", nullable(dictionary(model_health()))),
            ("model_settings", nullable(dictionary(model_settings()))),
            ("bedrock_config", nullable(bedrock())),
            ("is_full_url", nullable(boolean())),
        ],
    )
}

fn agent(required: &[&str]) -> Value {
    let env = array(object(
        &["name", "value"],
        &[("name", text()), ("value", text()), ("description", nullable(text()))],
    ));
    object(
        required,
        &[
            ("agent_id", id()),
            ("name", text()),
            ("command", text()),
            ("args", array(text())),
            ("env", env.clone()),
            ("icon", nullable(text())),
            ("advanced", nullable(advanced_agent())),
            ("command_override", nullable(text())),
            ("env_override", nullable(env)),
            ("enabled", boolean()),
        ],
    )
}

fn schedule() -> Value {
    json!({"oneOf":[
        object(&["kind","at_ms"], &[("kind",json!({"const":"at"})),("at_ms",integer()),("description",nullable(text()))]),
        object(&["kind","every_ms"], &[("kind",json!({"const":"every"})),("every_ms",integer()),("description",nullable(text()))]),
        object(&["kind","expr"], &[("kind",json!({"const":"cron"})),("expr",text()),("tz",nullable(text())),("description",nullable(text()))])
    ]})
}
fn cron(required: &[&str]) -> Value {
    object(
        required,
        &[
            ("job_id", id()),
            ("name", text()),
            ("schedule", schedule()),
            ("description", nullable(text())),
            ("message", nullable(text())),
            ("prompt", nullable(text())),
            ("conversation_id", id()),
            ("created_by", text()),
            ("conversation_title", nullable(text())),
            ("execution_mode", nullable(text())),
            ("enabled", nullable(boolean())),
            ("queue_enabled", nullable(boolean())),
            ("max_retries", nullable(integer())),
            ("agent_config", nullable(cron_agent())),
        ],
    )
}

fn mcp_import() -> Value {
    let mut external = object(
        &[],
        &[
            ("name", id()),
            ("transport", super::config_operations::mcp_transport_schema()),
            ("command", id()),
            ("args", array(text())),
            ("env", dictionary(text())),
            ("url", id()),
            ("headers", dictionary(text())),
            ("enabled", boolean()),
            ("description", nullable(text())),
            (
                "type",
                json!({"enum":["stdio","http","sse","remote","streamable_http","streamable-http"]}),
            ),
        ],
    );
    external["anyOf"] = json!([{"required":["transport"]},{"required":["command"]},{"required":["url"]}]);
    let mut item = external.clone();
    item["required"] = json!(["name"]);
    let list = json!({"type":"array","minItems":1,"items":item});
    // The existing parser prioritizes servers, then mcpServers, then mcp. It also accepts arrays.
    json!({"anyOf":[list.clone(), object(&["servers"], &[("servers",list)]),
        object(&["mcp"], &[("mcp",dictionary(external.clone()))]),
        object(&["mcpServers"], &[("mcpServers",dictionary(external))])]})
}

pub(super) fn schema(path: &str) -> Value {
    match path {
        "capabilities"
        | "context"
        | "assistants.list"
        | "skills.list"
        | "skills.paths"
        | "skills.external-paths.list"
        | "skills.market.enable"
        | "skills.market.disable"
        | "mcp.servers.list"
        | "mcp.agent-configs"
        | "mcp.oauth.authenticated"
        | "providers.list"
        | "settings.get"
        | "settings.client.get"
        | "agents.list"
        | "cron.jobs.list"
        | "cron.current.list" => object(&[], &[]),
        "conversation.rename" => object(
            &["conversation_id", "name"],
            &[("conversation_id", id()), ("name", text())],
        ),
        "assistants.get"
        | "assistants.delete"
        | "assistants.rule.read"
        | "assistants.rule.delete"
        | "assistants.skill.read"
        | "assistants.skill.delete" => object(
            &["assistant_id"],
            &[("assistant_id", id()), ("locale", nullable(text()))],
        ),
        "assistants.rule.write" | "assistants.skill.write" => object(
            &["assistant_id", "content"],
            &[
                ("assistant_id", id()),
                ("locale", nullable(text())),
                ("content", text()),
            ],
        ),
        "assistants.create" => {
            let mut s = assistant(&["name"]);
            s["properties"]["name"] = text();
            s
        }
        "assistants.update" => assistant(&["assistant_id"]),
        "assistants.import" => object(&["assistants"], &[("assistants", array(schema("assistants.create")))]),
        "assistants.state" => object(
            &["assistant_id"],
            &[
                ("assistant_id", id()),
                ("enabled", nullable(boolean())),
                ("sort_order", nullable(integer())),
                ("last_used_at", nullable(integer())),
            ],
        ),
        "skills.info" | "skills.import" => object(&["skill_path"], &[("skill_path", id())]),
        "skills.delete" => object(&["skill_name"], &[("skill_name", id())]),
        "skills.scan" => object(&["folder_path"], &[("folder_path", id())]),
        "skills.external-paths.add" => object(&["name", "path"], &[("name", text()), ("path", id())]),
        "skills.external-paths.remove" => object(&["path"], &[("path", id())]),
        "mcp.servers.get" | "mcp.servers.delete" | "mcp.servers.toggle" => {
            object(&["server_id"], &[("server_id", id())])
        }
        "mcp.servers.create" | "mcp.test-connection" => object(
            &["name", "transport"],
            &[
                ("name", id()),
                ("transport", super::config_operations::mcp_transport_schema()),
                ("description", nullable(text())),
                ("original_json", nullable(text())),
                ("builtin", boolean()),
            ],
        ),
        "mcp.servers.update" => object(
            &["server_id"],
            &[
                ("server_id", id()),
                ("name", nullable(text())),
                ("description", nullable(text())),
                ("original_json", nullable(text())),
                ("builtin", nullable(boolean())),
                ("transport", nullable(super::config_operations::mcp_transport_schema())),
            ],
        ),
        "mcp.servers.import" => mcp_import(),
        "mcp.oauth.check-status" | "mcp.oauth.login" | "mcp.oauth.logout" => {
            object(&["server_url"], &[("server_url", id())])
        }
        "providers.create" => {
            let mut s = provider(&["name", "platform", "base_url", "api_key"]);
            for field in ["name", "platform", "base_url", "api_key"] {
                s["properties"][field] = text();
            }
            for (field, rule) in [
                ("models", array(text())),
                ("enabled", boolean()),
                ("capabilities", array(capability())),
                ("model_settings", dictionary(model_settings())),
                ("is_full_url", boolean()),
            ] {
                s["properties"][field] = rule;
            }
            s
        }
        "providers.update" => provider(&["provider_id"]),
        "providers.delete" => object(&["provider_id"], &[("provider_id", id())]),
        "providers.models.fetch" => object(&["provider_id"], &[("provider_id", id()), ("try_fix", boolean())]),
        "providers.health-check" => object(&["provider_id", "model"], &[("provider_id", id()), ("model", text())]),
        "providers.detect-protocol" => object(
            &["base_url", "api_key"],
            &[
                ("base_url", text()),
                ("api_key", text()),
                ("timeout", nullable(json!({"type":"integer","minimum":0}))),
                ("test_all_keys", boolean()),
                (
                    "preferred_protocol",
                    nullable(json!({"enum":["openai","anthropic","gemini","unknown"]})),
                ),
            ],
        ),
        "providers.fetch-models" => object(
            &["platform", "base_url", "api_key"],
            &[
                ("platform", text()),
                ("base_url", text()),
                ("api_key", text()),
                ("bedrock_config", nullable(bedrock())),
                ("try_fix", boolean()),
            ],
        ),
        "settings.patch" => object(
            &[],
            &[
                ("language", nullable(text())),
                ("notification_enabled", nullable(boolean())),
                ("cron_notification_enabled", nullable(boolean())),
                ("command_queue_enabled", nullable(boolean())),
                ("save_upload_to_workspace", nullable(boolean())),
                ("cross_session_message_enabled", nullable(boolean())),
            ],
        ),
        "settings.client.put" => object(&[], &[]),
        "agents.enable" => {
            let mut s = agent(&["agent_id", "enabled"]);
            s["properties"]["enabled"] = boolean();
            s
        }
        "agents.overrides.get" | "agents.overrides.set" | "agents.custom.delete" => agent(&["agent_id"]),
        "agents.custom.create" => agent(&["name", "command"]),
        "agents.custom.update" => agent(&["agent_id", "name", "command"]),
        "agents.custom.try-connect" => object(
            &["command"],
            &[
                ("command", text()),
                ("acp_args", array(text())),
                ("env", dictionary(text())),
                ("runtime_scope_id", nullable(text())),
            ],
        ),
        "cron.jobs.get" | "cron.jobs.delete" | "cron.jobs.run" | "cron.jobs.skill.get" | "cron.jobs.skill.delete" => {
            object(&["job_id"], &[("job_id", id())])
        }
        "cron.jobs.skill.save" => object(&["job_id", "content"], &[("job_id", id()), ("content", text())]),
        "cron.jobs.create" => {
            let mut schema = cron(&["name", "schedule", "conversation_id", "created_by"]);
            schema["properties"]["queue_enabled"] = boolean();
            schema
        }
        "cron.jobs.update" => {
            let mut s = cron(&["job_id"]);
            for field in ["name", "schedule"] {
                s["properties"][field] = nullable(s["properties"][field].clone());
            }
            s
        }
        "cron.current.create" | "cron.current.update" => {
            let mut required = vec!["name", "schedule"];
            if path.ends_with("update") {
                required.push("job_id");
            }
            object(
                &required,
                &[
                    ("job_id", id()),
                    ("name", text()),
                    ("schedule", text()),
                    ("schedule_description", text()),
                    ("message", text()),
                ],
            )
        }
        _ => panic!("config operation is missing a public input contract: {path}"),
    }
}
