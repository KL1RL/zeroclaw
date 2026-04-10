use super::traits::{Tool, ToolResult};
use crate::agent::tool_execution::current_tool_channel_context;
use crate::config::HomeAssistantConfig;
use crate::home_assistant_client::{HomeAssistantClient, load_runtime_context_and_config};
use crate::security::SecurityPolicy;
use crate::security::policy::ToolOperation;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub struct HomeAssistantTool {
    security: Arc<SecurityPolicy>,
    runtime_dirs_override: Option<(PathBuf, PathBuf)>,
}

impl HomeAssistantTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            runtime_dirs_override: None,
        }
    }

    #[cfg(test)]
    fn new_with_runtime_dirs(
        security: Arc<SecurityPolicy>,
        config_dir: PathBuf,
        workspace_dir: PathBuf,
    ) -> Self {
        Self {
            security,
            runtime_dirs_override: Some((config_dir, workspace_dir)),
        }
    }

    async fn runtime_dirs(&self) -> anyhow::Result<(PathBuf, PathBuf)> {
        if let Some((config_dir, workspace_dir)) = &self.runtime_dirs_override {
            return Ok((config_dir.clone(), workspace_dir.clone()));
        }

        crate::config::schema::resolve_runtime_dirs_for_onboarding().await
    }

    async fn client_and_config(
        &self,
    ) -> anyhow::Result<(HomeAssistantClient, HomeAssistantConfig)> {
        let (config_dir, workspace_dir) = self.runtime_dirs().await?;
        let (context, config) =
            load_runtime_context_and_config(&config_dir, &workspace_dir).await?;
        Ok((HomeAssistantClient::from_context(context)?, config))
    }

    fn error_result(message: impl Into<String>) -> ToolResult {
        ToolResult {
            success: false,
            output: String::new(),
            error: Some(message.into()),
        }
    }

    fn ok_result(value: Value) -> ToolResult {
        ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            error: None,
        }
    }

    fn validate_return_response(args: &Value) -> anyhow::Result<Option<bool>> {
        match args.get("return_response") {
            Some(value) => Ok(Some(value.as_bool().ok_or_else(|| {
                anyhow::anyhow!("'return_response' must be a boolean when provided")
            })?)),
            None => Ok(None),
        }
    }

    fn ensure_read_allowed(action: &str) -> anyhow::Result<()> {
        match action {
            "list_services" | "list_states" | "get_state" => Ok(()),
            _ => anyhow::bail!("Unknown read action: {action}"),
        }
    }

    fn ensure_write_allowed(
        config: &HomeAssistantConfig,
        domain: &str,
        service: &str,
        entity_id: Option<&str>,
        data: Option<&Value>,
        target: Option<&Value>,
    ) -> anyhow::Result<()> {
        let context = current_tool_channel_context().ok_or_else(|| {
            anyhow::anyhow!(
                "Home Assistant writes are denied outside channel tool execution context"
            )
        })?;
        let reply_target = context.reply_target.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Home Assistant writes require a channel reply target and are denied here"
            )
        })?;

        let write_channel_allowed = config.write_channels.iter().any(|rule| {
            rule.channel.eq_ignore_ascii_case(&context.channel_name)
                && rule
                    .channel_ids
                    .iter()
                    .any(|candidate| candidate == reply_target)
        });
        if !write_channel_allowed {
            anyhow::bail!(
                "Home Assistant write denied: channel '{}' and channel ID '{}' are not allowed",
                context.channel_name,
                reply_target
            );
        }

        if !config
            .allowed_domains
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(domain))
        {
            anyhow::bail!(
                "Home Assistant write denied: domain '{}' is not in home_assistant.allowed_domains",
                domain
            );
        }

        if !config
            .allowed_services
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(service))
        {
            anyhow::bail!(
                "Home Assistant write denied: service '{}' is not in home_assistant.allowed_services",
                service
            );
        }

        let entity_ids = collect_write_entity_ids(entity_id, data, target)?;
        if entity_ids.is_empty() {
            anyhow::bail!(
                "Home Assistant write denied: call_service requires an explicit entity_id target"
            );
        }

        for entity_id in entity_ids {
            if !config
                .allowed_entity_ids
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&entity_id))
            {
                anyhow::bail!(
                    "Home Assistant write denied: entity_id '{}' is not in home_assistant.allowed_entity_ids",
                    entity_id
                );
            }
        }

        Ok(())
    }
}

fn collect_write_entity_ids(
    entity_id: Option<&str>,
    data: Option<&Value>,
    target: Option<&Value>,
) -> anyhow::Result<Vec<String>> {
    if let Some(data) = data.and_then(Value::as_object) {
        if data.contains_key("entity_id")
            || data.contains_key("area_id")
            || data.contains_key("device_id")
        {
            anyhow::bail!(
                "Home Assistant writes must target explicit entity_id values via top-level entity_id or target.entity_id"
            );
        }
    }

    let mut entity_ids = Vec::new();
    if let Some(entity_id) = entity_id {
        entity_ids.push(normalize_entity_id(entity_id)?);
    }

    if let Some(target) = target {
        let target = target
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("'target' must be an object when provided"))?;
        if target.keys().any(|key| key != "entity_id") {
            anyhow::bail!(
                "Home Assistant writes only allow target.entity_id; area_id and device_id are denied"
            );
        }

        if let Some(value) = target.get("entity_id") {
            match value {
                Value::String(entity_id) => entity_ids.push(normalize_entity_id(entity_id)?),
                Value::Array(items) => {
                    for item in items {
                        let entity_id = item.as_str().ok_or_else(|| {
                            anyhow::anyhow!("target.entity_id array entries must be strings")
                        })?;
                        entity_ids.push(normalize_entity_id(entity_id)?);
                    }
                }
                _ => anyhow::bail!("target.entity_id must be a string or string array"),
            }
        }
    }

    entity_ids.sort_unstable();
    entity_ids.dedup();
    Ok(entity_ids)
}

fn normalize_entity_id(entity_id: &str) -> anyhow::Result<String> {
    let trimmed = entity_id.trim();
    if trimmed.is_empty() {
        anyhow::bail!("entity_id must not be empty");
    }
    if !trimmed.contains('.') {
        anyhow::bail!("entity_id must include a domain prefix like 'light.kitchen'");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    {
        anyhow::bail!(
            "Invalid entity_id: only alphanumeric, underscore, dot, and hyphen are allowed"
        );
    }
    Ok(trimmed.to_string())
}

#[async_trait]
impl Tool for HomeAssistantTool {
    fn name(&self) -> &str {
        "home_assistant"
    }

    fn description(&self) -> &str {
        "Interact with Home Assistant via its REST API. Read operations are limited to services and entity state lookups when [home_assistant] is configured. Write operations are denied by default and only allowed for configured domains, services, entity IDs, and channel/channel-ID pairs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_services", "list_states", "get_state", "call_service"],
                    "description": "Home Assistant action to perform"
                },
                "domain": {
                    "type": "string",
                    "description": "Domain name for list_states filtering or call_service (e.g. light, switch, climate)"
                },
                "entity_id": {
                    "type": "string",
                    "description": "Entity ID for get_state, or shorthand target for call_service (e.g. light.kitchen)"
                },
                "service": {
                    "type": "string",
                    "description": "Service name for call_service (e.g. turn_on, turn_off, set_temperature)"
                },
                "data": {
                    "type": "object",
                    "description": "Optional extra service payload for call_service. Write-restricted mode forbids putting entity selectors in data."
                },
                "target": {
                    "type": "object",
                    "description": "Optional call_service target object. Write-restricted mode only allows target.entity_id."
                },
                "return_response": {
                    "type": "boolean",
                    "description": "Only set true when the Home Assistant service explicitly supports response data; otherwise omit it"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = match args.get("action").and_then(Value::as_str) {
            Some(action) => action,
            None => return Ok(Self::error_result("Missing required parameter: action")),
        };

        let operation = match action {
            "list_services" | "list_states" | "get_state" => ToolOperation::Read,
            "call_service" => ToolOperation::Act,
            _ => {
                return Ok(Self::error_result(format!(
                    "Unknown action: {action}. Valid actions: list_services, list_states, get_state, call_service"
                )));
            }
        };

        if let Err(error) = self
            .security
            .enforce_tool_operation(operation, &format!("home_assistant.{action}"))
        {
            return Ok(Self::error_result(error));
        }

        let (client, config) = match self.client_and_config().await {
            Ok(value) => value,
            Err(error) => return Ok(Self::error_result(error.to_string())),
        };

        let result: anyhow::Result<Value> = match action {
            "list_services" => {
                if let Err(error) = Self::ensure_read_allowed(action) {
                    Err(error)
                } else {
                    client.list_services().await
                }
            }
            "list_states" => {
                if let Err(error) = Self::ensure_read_allowed(action) {
                    Err(error)
                } else {
                    client
                        .list_states(args.get("domain").and_then(Value::as_str))
                        .await
                }
            }
            "get_state" => {
                if let Err(error) = Self::ensure_read_allowed(action) {
                    return Ok(Self::error_result(error.to_string()));
                }
                let Some(entity_id) = args.get("entity_id").and_then(Value::as_str) else {
                    return Ok(Self::error_result("get_state requires entity_id parameter"));
                };
                client.get_state(entity_id).await
            }
            "call_service" => {
                let Some(domain) = args.get("domain").and_then(Value::as_str) else {
                    return Ok(Self::error_result("call_service requires domain parameter"));
                };
                let Some(service) = args.get("service").and_then(Value::as_str) else {
                    return Ok(Self::error_result(
                        "call_service requires service parameter",
                    ));
                };
                let entity_id = args.get("entity_id").and_then(Value::as_str);
                let data = args.get("data");
                let target = args.get("target");
                let return_response = match Self::validate_return_response(&args) {
                    Ok(value) => value,
                    Err(error) => return Ok(Self::error_result(error.to_string())),
                };
                if let Err(error) =
                    Self::ensure_write_allowed(&config, domain, service, entity_id, data, target)
                {
                    Err(error)
                } else {
                    client
                        .call_service(domain, service, entity_id, data, target, return_response)
                        .await
                }
            }
            _ => unreachable!(),
        };

        match result {
            Ok(value) => Ok(Self::ok_result(value)),
            Err(error) => Ok(Self::error_result(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool_execution::scope_tool_channel_context;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use tempfile::TempDir;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    #[tokio::test]
    async fn get_state_uses_shared_client_path() {
        let temp = TempDir::new().unwrap();
        let active_config_dir = temp.path().join("profiles").join("alpha");
        let workspace_dir = active_config_dir.join("workspace");
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/states/light.kitchen"))
            .and(header("authorization", "Bearer test-ha-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "entity_id": "light.kitchen",
                "state": "on"
            })))
            .mount(&server)
            .await;

        tokio::fs::write(
            active_config_dir.join("config.toml"),
            format!(
                "[home_assistant]\nenabled = true\nurl = \"{}\"\ndisable_strict_ssl = false\n",
                server.uri()
            ),
        )
        .await
        .unwrap();
        tokio::fs::write(
            workspace_dir.join(".env"),
            "HOME_ASSISTANT_ACCESS_TOKEN=test-ha-token\n",
        )
        .await
        .unwrap();

        let tool = HomeAssistantTool::new_with_runtime_dirs(
            test_security(),
            active_config_dir,
            workspace_dir,
        );

        let result = tool
            .execute(json!({
                "action": "get_state",
                "entity_id": "light.kitchen"
            }))
            .await
            .unwrap();

        assert!(result.success, "expected success, got {:?}", result.error);
        assert!(result.output.contains("light.kitchen"));
    }

    #[tokio::test]
    async fn call_service_is_denied_by_default() {
        let temp = TempDir::new().unwrap();
        let active_config_dir = temp.path().join("profiles").join("alpha");
        let workspace_dir = active_config_dir.join("workspace");
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();

        tokio::fs::write(
            active_config_dir.join("config.toml"),
            "[home_assistant]\nenabled = true\nurl = \"http://ha.local:8123\"\ndisable_strict_ssl = false\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            workspace_dir.join(".env"),
            "HOME_ASSISTANT_ACCESS_TOKEN=test-ha-token\n",
        )
        .await
        .unwrap();

        let tool = HomeAssistantTool::new_with_runtime_dirs(
            test_security(),
            active_config_dir,
            workspace_dir,
        );

        let result = scope_tool_channel_context(
            "slack",
            Some("C123"),
            tool.execute(json!({
                "action": "call_service",
                "domain": "light",
                "service": "turn_on",
                "entity_id": "light.kitchen"
            })),
        )
        .await
        .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn call_service_requires_matching_channel_and_entity_allowlists() {
        let temp = TempDir::new().unwrap();
        let active_config_dir = temp.path().join("profiles").join("alpha");
        let workspace_dir = active_config_dir.join("workspace");
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/services/light/turn_off"))
            .and(header("authorization", "Bearer test-ha-token"))
            .and(body_json(json!({
                "entity_id": "light.kitchen"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        tokio::fs::write(
            active_config_dir.join("config.toml"),
            format!(
                "[home_assistant]\nenabled = true\nurl = \"{}\"\ndisable_strict_ssl = false\nallowed_domains = [\"light\"]\nallowed_services = [\"turn_off\"]\nallowed_entity_ids = [\"light.kitchen\"]\n\n[[home_assistant.write_channels]]\nchannel = \"slack\"\nchannel_ids = [\"C123\"]\n",
                server.uri()
            ),
        )
        .await
        .unwrap();
        tokio::fs::write(
            workspace_dir.join(".env"),
            "HOME_ASSISTANT_ACCESS_TOKEN=test-ha-token\n",
        )
        .await
        .unwrap();

        let tool = HomeAssistantTool::new_with_runtime_dirs(
            test_security(),
            active_config_dir,
            workspace_dir,
        );

        let result = scope_tool_channel_context(
            "slack",
            Some("C123"),
            tool.execute(json!({
                "action": "call_service",
                "domain": "light",
                "service": "turn_off",
                "entity_id": "light.kitchen"
            })),
        )
        .await
        .unwrap();

        assert!(result.success, "expected success, got {:?}", result.error);
    }
}
