use super::traits::{Tool, ToolResult};
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use anyhow::Context;
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const HOME_ASSISTANT_TIMEOUT_SECS: u64 = 30;
const MAX_ERROR_BODY_CHARS: usize = 500;
const HOME_ASSISTANT_TOKEN_KEYS: &[&str] = &[
    "HOME_ASSISTANT_ACCESS_TOKEN",
    "HOME_ASSISTANT_TOKEN",
    "HOME_ASSISTANT_API_KEY",
];

#[derive(Debug, Clone)]
struct RuntimeContext {
    base_url: reqwest::Url,
    token: String,
    disable_strict_ssl: bool,
}

/// Home Assistant REST API integration.
///
/// Runtime context is resolved lazily on each execution so the tool follows the
/// current active workspace marker and reads credentials from that workspace's
/// `.env` file.
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

    async fn runtime_context(&self) -> anyhow::Result<RuntimeContext> {
        let (config_dir, workspace_dir) = self.runtime_dirs().await?;
        let config_path = config_dir.join("config.toml");
        let contents = tokio::fs::read_to_string(&config_path)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Failed to read Home Assistant config from {}: {error}",
                    config_path.display()
                )
            })?;
        let config: crate::config::Config = toml::from_str(&contents).map_err(|error| {
            anyhow::anyhow!(
                "Failed to parse Home Assistant config from {}: {error}",
                config_path.display()
            )
        })?;

        if !config.home_assistant.enabled {
            anyhow::bail!(
                "Home Assistant tool is disabled in {}. Enable [home_assistant].enabled first",
                config_path.display()
            );
        }

        config.home_assistant.validate()?;

        Ok(RuntimeContext {
            base_url: normalize_base_url(&config.home_assistant.url)?,
            token: read_home_assistant_token(&workspace_dir).await?,
            disable_strict_ssl: config.home_assistant.disable_strict_ssl,
        })
    }

    async fn runtime_dirs(&self) -> anyhow::Result<(PathBuf, PathBuf)> {
        if let Some((config_dir, workspace_dir)) = &self.runtime_dirs_override {
            return Ok((config_dir.clone(), workspace_dir.clone()));
        }

        crate::config::schema::resolve_runtime_dirs_for_onboarding().await
    }

    fn client(&self, context: &RuntimeContext) -> anyhow::Result<reqwest::Client> {
        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(HOME_ASSISTANT_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            .danger_accept_invalid_certs(context.disable_strict_ssl);
        let builder = crate::config::apply_runtime_proxy_to_builder(builder, "tool.home_assistant");
        builder.build().map_err(Into::into)
    }

    async fn send_json(
        &self,
        request: reqwest::RequestBuilder,
        action_label: &str,
    ) -> anyhow::Result<Value> {
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&body, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("Home Assistant {action_label} failed ({status}): {truncated}");
        }
        response.json().await.map_err(Into::into)
    }

    async fn list_states(
        &self,
        client: &reqwest::Client,
        context: &RuntimeContext,
        domain: Option<&str>,
    ) -> anyhow::Result<Value> {
        let url = context.base_url.join("api/states")?;
        let mut value = self
            .send_json(client.get(url).bearer_auth(&context.token), "list_states")
            .await?;

        if let Some(domain) = domain {
            let domain = validate_domain_or_service(domain, "domain")?;
            let filtered = value
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    entry
                        .get("entity_id")
                        .and_then(Value::as_str)
                        .and_then(|entity_id| entity_id.split_once('.'))
                        .is_some_and(|(entity_domain, _)| {
                            entity_domain.eq_ignore_ascii_case(&domain)
                        })
                })
                .collect::<Vec<_>>();
            value = Value::Array(filtered);
        }

        Ok(value)
    }

    async fn get_state(
        &self,
        client: &reqwest::Client,
        context: &RuntimeContext,
        entity_id: &str,
    ) -> anyhow::Result<Value> {
        let entity_id = validate_entity_id(entity_id)?;
        let url = context.base_url.join(&format!("api/states/{entity_id}"))?;
        self.send_json(client.get(url).bearer_auth(&context.token), "get_state")
            .await
    }

    async fn list_services(
        &self,
        client: &reqwest::Client,
        context: &RuntimeContext,
    ) -> anyhow::Result<Value> {
        let url = context.base_url.join("api/services")?;
        self.send_json(client.get(url).bearer_auth(&context.token), "list_services")
            .await
    }

    async fn get_config(
        &self,
        client: &reqwest::Client,
        context: &RuntimeContext,
    ) -> anyhow::Result<Value> {
        let url = context.base_url.join("api/config")?;
        self.send_json(client.get(url).bearer_auth(&context.token), "get_config")
            .await
    }

    async fn call_service(
        &self,
        client: &reqwest::Client,
        context: &RuntimeContext,
        domain: &str,
        service: &str,
        entity_id: Option<&str>,
        data: Option<&Value>,
        target: Option<&Value>,
        return_response: Option<bool>,
    ) -> anyhow::Result<Value> {
        let domain = validate_domain_or_service(domain, "domain")?;
        let service = validate_domain_or_service(service, "service")?;
        let mut url = context
            .base_url
            .join(&format!("api/services/{domain}/{service}"))?;
        if return_response == Some(true) {
            url.query_pairs_mut().append_pair("return_response", "1");
        }

        let mut body = Map::new();
        if let Some(data) = data {
            let object = data
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("'data' must be an object when provided"))?;
            for (key, value) in object {
                body.insert(key.clone(), value.clone());
            }
        }

        if let Some(entity_id) = entity_id {
            let entity_id = validate_entity_id(entity_id)?;
            body.insert("entity_id".to_string(), Value::String(entity_id));
        }

        if let Some(target) = target {
            let object = target
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("'target' must be an object when provided"))?;
            for (key, value) in object {
                body.insert(key.clone(), value.clone());
            }
        }

        self.send_json(
            client
                .post(url)
                .bearer_auth(&context.token)
                .json(&Value::Object(body)),
            "call_service",
        )
        .await
    }
}

#[async_trait]
impl Tool for HomeAssistantTool {
    fn name(&self) -> &str {
        "home_assistant"
    }

    fn description(&self) -> &str {
        "Interact with Home Assistant via its REST API using bearer-token auth and JSON payloads. Supports reading config, listing services, listing entity states, getting a single entity state, and calling services. For call_service, pass target keys like entity_id either directly or under target; target fields are flattened into the REST service payload. Put extra service fields under data, and omit return_response unless the service explicitly supports response data. For light dimming, call light.turn_on and pass brightness in data, usually brightness_pct as a 0-100 percentage. Reads the access token from the active workspace .env file."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get_config", "list_services", "list_states", "get_state", "call_service"],
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
                    "description": "Optional extra service payload for call_service; prefer top-level entity_id/target over duplicating target keys here. Example for dimming a light: {\"brightness_pct\":50} with service \"turn_on\""
                },
                "target": {
                    "type": "object",
                    "description": "Optional call_service target object; its keys are flattened into the REST payload, e.g. {\"entity_id\":\"light.kitchen\"}"
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
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required parameter: action".into()),
                });
            }
        };

        let operation = match action {
            "get_config" | "list_services" | "list_states" | "get_state" => ToolOperation::Read,
            "call_service" => ToolOperation::Act,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Unknown action: {action}. Valid actions: get_config, list_services, list_states, get_state, call_service"
                    )),
                });
            }
        };

        if let Err(error) = self
            .security
            .enforce_tool_operation(operation, &format!("home_assistant.{action}"))
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let context = match self.runtime_context().await {
            Ok(context) => context,
            Err(error) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(error.to_string()),
                });
            }
        };
        let client = self.client(&context)?;

        let result: anyhow::Result<Value> = match action {
            "get_config" => self.get_config(&client, &context).await,
            "list_services" => self.list_services(&client, &context).await,
            "list_states" => {
                let domain = args.get("domain").and_then(Value::as_str);
                self.list_states(&client, &context, domain).await
            }
            "get_state" => {
                let Some(entity_id) = args.get("entity_id").and_then(Value::as_str) else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("get_state requires entity_id parameter".into()),
                    });
                };
                self.get_state(&client, &context, entity_id).await
            }
            "call_service" => {
                let Some(domain) = args.get("domain").and_then(Value::as_str) else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("call_service requires domain parameter".into()),
                    });
                };
                let Some(service) = args.get("service").and_then(Value::as_str) else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("call_service requires service parameter".into()),
                    });
                };
                let return_response = match args.get("return_response") {
                    Some(value) => Some(value.as_bool().ok_or_else(|| {
                        anyhow::anyhow!("'return_response' must be a boolean when provided")
                    })?),
                    None => None,
                };
                self.call_service(
                    &client,
                    &context,
                    domain,
                    service,
                    args.get("entity_id").and_then(Value::as_str),
                    args.get("data"),
                    args.get("target"),
                    return_response,
                )
                .await
            }
            _ => unreachable!(),
        };

        match result {
            Ok(value) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
                error: None,
            }),
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
            }),
        }
    }
}

fn normalize_base_url(raw_url: &str) -> anyhow::Result<reqwest::Url> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        anyhow::bail!("home_assistant.url is empty");
    }

    let mut url = reqwest::Url::parse(trimmed)
        .with_context(|| "home_assistant.url must be a valid absolute URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("home_assistant.url must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("home_assistant.url must not embed credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("home_assistant.url must not include a query string or fragment");
    }

    let path = url.path().to_string();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }

    Ok(url)
}

fn parse_env_value(raw: &str) -> String {
    let raw = raw.trim();
    let without_comment = raw.split_once(" #").map_or(raw, |(value, _)| value).trim();
    let unquoted = if without_comment.len() >= 2
        && ((without_comment.starts_with('"') && without_comment.ends_with('"'))
            || (without_comment.starts_with('\'') && without_comment.ends_with('\'')))
    {
        &without_comment[1..without_comment.len() - 1]
    } else {
        without_comment
    };

    unquoted.trim().to_string()
}

async fn read_home_assistant_token(workspace_dir: &Path) -> anyhow::Result<String> {
    let env_path = workspace_dir.join(".env");
    if env_path.exists() {
        let content = tokio::fs::read_to_string(&env_path)
            .await
            .with_context(|| format!("Failed to read {}", env_path.display()))?;
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
            if let Some((key, value)) = line.split_once('=') {
                if HOME_ASSISTANT_TOKEN_KEYS
                    .iter()
                    .any(|candidate| key.trim().eq_ignore_ascii_case(candidate))
                {
                    let value = parse_env_value(value);
                    if !value.is_empty() {
                        return Ok(value);
                    }
                }
            }
        }
    }

    anyhow::bail!(
        "Home Assistant token not found. Set one of {} in the active workspace .env file",
        HOME_ASSISTANT_TOKEN_KEYS.join(", ")
    )
}

fn validate_domain_or_service(value: &str, field_name: &str) -> anyhow::Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{field_name} must not be empty");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        anyhow::bail!(
            "Invalid {field_name}: only lowercase alphanumeric, underscore, and hyphen are allowed"
        );
    }
    Ok(trimmed.to_string())
}

fn validate_entity_id(entity_id: &str) -> anyhow::Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn tool_metadata_is_present() {
        let tool = HomeAssistantTool::new_with_runtime_dirs(
            test_security(),
            std::env::temp_dir().join("ha-test-root"),
            std::env::temp_dir().join("ha-test-root-workspace"),
        );
        assert_eq!(tool.name(), "home_assistant");
        assert!(tool.description().contains("Home Assistant"));
        assert!(tool.parameters_schema()["properties"]["action"].is_object());
    }

    #[tokio::test]
    async fn get_state_uses_active_workspace_env_token() {
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
            active_config_dir.clone(),
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
        assert!(result.output.contains("\"on\""));
    }

    #[tokio::test]
    async fn call_service_respects_security_policy() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = HomeAssistantTool::new_with_runtime_dirs(
            security,
            std::env::temp_dir().join("ha-readonly-root"),
            std::env::temp_dir().join("ha-readonly-workspace"),
        );

        let result = tool
            .execute(json!({
                "action": "call_service",
                "domain": "light",
                "service": "turn_on"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
    }

    #[tokio::test]
    async fn call_service_accepts_entity_id_shorthand() {
        let temp = TempDir::new().unwrap();
        let active_config_dir = temp.path().join("profiles").join("alpha");
        let workspace_dir = active_config_dir.join("workspace");
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/services/light/turn_off"))
            .and(header("authorization", "Bearer test-ha-token"))
            .and(body_json(json!({
                "entity_id": "light.kitchen_island_chandelier"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
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
            active_config_dir.clone(),
            workspace_dir,
        );
        let result = tool
            .execute(json!({
                "action": "call_service",
                "domain": "light",
                "service": "turn_off",
                "entity_id": "light.kitchen_island_chandelier"
            }))
            .await
            .unwrap();

        assert!(result.success, "expected success, got {:?}", result.error);
    }

    #[tokio::test]
    async fn call_service_flattens_target_into_service_data() {
        let temp = TempDir::new().unwrap();
        let active_config_dir = temp.path().join("profiles").join("alpha");
        let workspace_dir = active_config_dir.join("workspace");
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/services/light/turn_on"))
            .and(header("authorization", "Bearer test-ha-token"))
            .and(body_json(json!({
                "entity_id": "light.kitchen_island_chandelier",
                "brightness": 128
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
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
            active_config_dir.clone(),
            workspace_dir,
        );
        let result = tool
            .execute(json!({
                "action": "call_service",
                "domain": "light",
                "service": "turn_on",
                "data": { "brightness": 128 },
                "target": { "entity_id": "light.kitchen_island_chandelier" }
            }))
            .await
            .unwrap();

        assert!(result.success, "expected success, got {:?}", result.error);
    }
}
