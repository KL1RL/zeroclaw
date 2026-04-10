use anyhow::Context;
use serde_json::{Map, Value};
use std::path::Path;
use std::time::Duration;

pub const HOME_ASSISTANT_TIMEOUT_SECS: u64 = 30;
pub const HOME_ASSISTANT_PROXY_SCOPE: &str = "tool.home_assistant";
pub const HOME_ASSISTANT_TOKEN_KEYS: &[&str] = &[
    "HOME_ASSISTANT_ACCESS_TOKEN",
    "HOME_ASSISTANT_TOKEN",
    "HOME_ASSISTANT_API_KEY",
];

const MAX_ERROR_BODY_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct HomeAssistantRuntimeContext {
    pub base_url: reqwest::Url,
    pub token: String,
    pub disable_strict_ssl: bool,
}

/// Shared Home Assistant REST client used by the built-in tool and debug CLI.
#[derive(Debug, Clone)]
pub struct HomeAssistantClient {
    client: reqwest::Client,
    context: HomeAssistantRuntimeContext,
}

impl HomeAssistantClient {
    pub fn new(
        base_url: &str,
        token: impl Into<String>,
        disable_strict_ssl: bool,
    ) -> anyhow::Result<Self> {
        Self::from_parts(
            normalize_base_url(base_url)?,
            token.into(),
            disable_strict_ssl,
            HOME_ASSISTANT_PROXY_SCOPE,
        )
    }

    pub fn from_context(context: HomeAssistantRuntimeContext) -> anyhow::Result<Self> {
        Self::from_parts(
            context.base_url.clone(),
            context.token.clone(),
            context.disable_strict_ssl,
            HOME_ASSISTANT_PROXY_SCOPE,
        )
    }

    pub fn from_parts(
        base_url: reqwest::Url,
        token: String,
        disable_strict_ssl: bool,
        proxy_scope: &str,
    ) -> anyhow::Result<Self> {
        let token = token.trim().to_string();
        if token.is_empty() {
            anyhow::bail!("Home Assistant token must not be empty");
        }

        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(HOME_ASSISTANT_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            .danger_accept_invalid_certs(disable_strict_ssl);
        let builder = crate::config::apply_runtime_proxy_to_builder(builder, proxy_scope);

        Ok(Self {
            client: builder.build()?,
            context: HomeAssistantRuntimeContext {
                base_url,
                token,
                disable_strict_ssl,
            },
        })
    }

    pub async fn list_states(&self, domain: Option<&str>) -> anyhow::Result<Value> {
        let url = self.context.base_url.join("api/states")?;
        let mut value = self
            .send_json(
                self.client.get(url).bearer_auth(&self.context.token),
                "list_states",
            )
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

    pub async fn get_state(&self, entity_id: &str) -> anyhow::Result<Value> {
        let entity_id = validate_entity_id(entity_id)?;
        let url = self
            .context
            .base_url
            .join(&format!("api/states/{entity_id}"))?;
        self.send_json(
            self.client.get(url).bearer_auth(&self.context.token),
            "get_state",
        )
        .await
    }

    pub async fn list_services(&self) -> anyhow::Result<Value> {
        let url = self.context.base_url.join("api/services")?;
        self.send_json(
            self.client.get(url).bearer_auth(&self.context.token),
            "list_services",
        )
        .await
    }

    pub async fn get_config(&self) -> anyhow::Result<Value> {
        let url = self.context.base_url.join("api/config")?;
        self.send_json(
            self.client.get(url).bearer_auth(&self.context.token),
            "get_config",
        )
        .await
    }

    pub async fn call_service(
        &self,
        domain: &str,
        service: &str,
        entity_id: Option<&str>,
        data: Option<&Value>,
        target: Option<&Value>,
        return_response: Option<bool>,
    ) -> anyhow::Result<Value> {
        let domain = validate_domain_or_service(domain, "domain")?;
        let service = validate_domain_or_service(service, "service")?;
        let mut url = self
            .context
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
            // Home Assistant service calls expect target selectors inside the top-level
            // service data payload rather than under a nested `target` object.
            for (key, value) in object {
                body.insert(key.clone(), value.clone());
            }
        }

        self.send_json(
            self.client
                .post(url)
                .bearer_auth(&self.context.token)
                .json(&Value::Object(body)),
            "call_service",
        )
        .await
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
}

pub async fn resolve_runtime_context() -> anyhow::Result<HomeAssistantRuntimeContext> {
    let (config_dir, workspace_dir) =
        crate::config::schema::resolve_runtime_dirs_for_onboarding().await?;
    Ok(load_runtime_context_and_config(&config_dir, &workspace_dir)
        .await?
        .0)
}

pub async fn load_runtime_context(
    config_dir: &Path,
    workspace_dir: &Path,
) -> anyhow::Result<HomeAssistantRuntimeContext> {
    Ok(load_runtime_context_and_config(config_dir, workspace_dir)
        .await?
        .0)
}

pub async fn load_runtime_context_and_config(
    config_dir: &Path,
    workspace_dir: &Path,
) -> anyhow::Result<(
    HomeAssistantRuntimeContext,
    crate::config::HomeAssistantConfig,
)> {
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

    let home_assistant = config.home_assistant;
    home_assistant.validate()?;

    Ok((
        HomeAssistantRuntimeContext {
            base_url: normalize_base_url(&home_assistant.url)?,
            token: read_home_assistant_token(workspace_dir).await?,
            disable_strict_ssl: home_assistant.disable_strict_ssl,
        },
        home_assistant,
    ))
}

pub fn normalize_base_url(raw_url: &str) -> anyhow::Result<reqwest::Url> {
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

pub async fn read_home_assistant_token(workspace_dir: &Path) -> anyhow::Result<String> {
    read_home_assistant_token_file(&workspace_dir.join(".env")).await
}

pub async fn read_home_assistant_token_file(env_path: &Path) -> anyhow::Result<String> {
    if env_path.exists() {
        let content = tokio::fs::read_to_string(env_path)
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
        "Home Assistant token not found. Set one of {} in the environment or token file",
        HOME_ASSISTANT_TOKEN_KEYS.join(", ")
    )
}

pub fn read_home_assistant_token_from_environment() -> anyhow::Result<String> {
    for key in HOME_ASSISTANT_TOKEN_KEYS {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }

    anyhow::bail!(
        "Home Assistant token not found. Set one of {} in the environment or use -t to read a token file",
        HOME_ASSISTANT_TOKEN_KEYS.join(", ")
    )
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
    use tempfile::TempDir;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn token_file_supports_aliases_and_comments() {
        let temp = TempDir::new().unwrap();
        let env_path = temp.path().join(".env");
        tokio::fs::write(
            &env_path,
            "# comment\nexport HOME_ASSISTANT_TOKEN='token-value' # trailing\n",
        )
        .await
        .unwrap();

        let token = read_home_assistant_token_file(&env_path).await.unwrap();
        assert_eq!(token, "token-value");
    }

    #[tokio::test]
    async fn call_service_accepts_entity_id_shorthand() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/services/light/turn_off"))
            .and(header("authorization", "Bearer test-ha-token"))
            .and(body_json(serde_json::json!({
                "entity_id": "light.kitchen_island_chandelier"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = HomeAssistantClient::new(&server.uri(), "test-ha-token", false).unwrap();
        let value = client
            .call_service(
                "light",
                "turn_off",
                Some("light.kitchen_island_chandelier"),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(value, serde_json::json!([]));
    }

    #[tokio::test]
    async fn call_service_flattens_target_into_service_data() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/services/light/turn_on"))
            .and(header("authorization", "Bearer test-ha-token"))
            .and(body_json(serde_json::json!({
                "entity_id": "light.kitchen_island_chandelier",
                "brightness": 128
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = HomeAssistantClient::new(&server.uri(), "test-ha-token", false).unwrap();
        let value = client
            .call_service(
                "light",
                "turn_on",
                None,
                Some(&serde_json::json!({ "brightness": 128 })),
                Some(&serde_json::json!({ "entity_id": "light.kitchen_island_chandelier" })),
                None,
            )
            .await
            .unwrap();

        assert_eq!(value, serde_json::json!([]));
    }
}
