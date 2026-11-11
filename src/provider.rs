use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, ProviderConfig, ProviderKind, is_loopback_host};

const MAX_RESPONSE_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_STREAM_EVENT_BYTES: usize = 1024 * 1024;
const MAX_STREAM_TOOL_CALLS: usize = 256;
const MAX_GENERATED_TEXT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: Option<bool>,
    pub structured_output: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub capabilities: ModelCapabilities,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub prompt: String,
    pub messages: Vec<ChatMessage>,
    pub json_schema: Option<Value>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

impl ChatRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub provider: String,
    pub model: String,
    pub content: String,
    pub usage: Usage,
    pub tool_calls: Vec<ToolCallRequest>,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn identity(&self) -> &str;
    async fn discover_models(&self) -> Result<Vec<ModelInfo>>;
    async fn check(&self) -> Result<()>;
    async fn generate(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ChatResponse>;
}

pub fn create_provider(config: &AppConfig, name: &str) -> Result<Box<dyn ModelProvider>> {
    create_provider_with_environment(config, name, |variable| std::env::var(variable))
}

fn create_provider_with_environment(
    config: &AppConfig,
    name: &str,
    get_environment: impl Fn(&str) -> std::result::Result<String, std::env::VarError>,
) -> Result<Box<dyn ModelProvider>> {
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.name == name)
        .with_context(|| format!("Provider '{name}' is not configured"))?;
    validate_provider(provider)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(provider.timeout_secs))
        .build()
        .context("Could not initialize HTTP client")?;
    match provider.kind {
        ProviderKind::OpenaiCompatible => {
            let key = api_key(provider, &get_environment)?;
            Ok(Box::new(OpenAiCompatible::new(
                provider.clone(),
                client,
                key,
            )?))
        }
        ProviderKind::Ollama => Ok(Box::new(Ollama::new(provider.clone(), client)?)),
        ProviderKind::Anthropic => {
            let key = api_key(provider, &get_environment)?
                .context("Anthropic requires an API key environment variable")?;
            Ok(Box::new(Anthropic::new(provider.clone(), client, key)?))
        }
        ProviderKind::Gemini => {
            let key = api_key(provider, &get_environment)?
                .context("Google Gemini requires an API key environment variable")?;
            Ok(Box::new(Gemini::new(provider.clone(), client, key)?))
        }
    }
}

fn validate_provider(provider: &ProviderConfig) -> Result<()> {
    if provider.name.trim().is_empty() {
        bail!("Provider name must not be empty");
    }
    if provider.timeout_secs == 0 {
        bail!("Provider timeout must be greater than zero");
    }
    let url = Url::parse(&provider.base_url)
        .with_context(|| format!("Invalid endpoint for provider '{}'", provider.name))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "Provider endpoint must be an HTTP(S) URL without embedded credentials, query, or fragment"
        );
    }
    if provider.api_key_env.is_some()
        && url.scheme() == "http"
        && !url.host_str().is_some_and(is_loopback_host)
    {
        bail!(
            "Provider '{}' uses an API key with a non-loopback HTTP endpoint; use HTTPS to protect credentials",
            provider.name
        );
    }
    Ok(())
}

fn api_key(
    provider: &ProviderConfig,
    get_environment: &impl Fn(&str) -> std::result::Result<String, std::env::VarError>,
) -> Result<Option<String>> {
    let Some(variable) = provider.api_key_env.as_deref() else {
        return Ok(None);
    };
    if variable.trim().is_empty() {
        bail!("api_key_env must name a non-empty environment variable");
    }
    let value = get_environment(variable).with_context(|| {
        format!(
            "Provider '{}' requires environment variable '{}' to be set",
            provider.name, variable
        )
    })?;
    if value.is_empty() {
        bail!(
            "Provider '{}' requires a non-empty value in environment variable '{}'",
            provider.name,
            variable
        );
    }
    Ok(Some(value))
}

fn redact(text: &str, secret: Option<&str>) -> String {
    match secret.filter(|secret| !secret.is_empty()) {
        Some(secret) => text.replace(secret, "[REDACTED]"),
        None => text.to_owned(),
    }
}

fn endpoint(base: &str, path: &str) -> Result<Url> {
    let mut url = Url::parse(base).context("Invalid provider URL")?;
    let prefix = url.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}/{path}"));
    Ok(url)
}

fn client_error(error: reqwest::Error, secret: Option<&str>) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("Provider request timed out")
    } else {
        anyhow::anyhow!("{}", redact(&error.to_string(), secret))
    }
}

struct OpenAiCompatible {
    config: ProviderConfig,
    client: Client,
    api_key: Option<String>,
}

impl OpenAiCompatible {
    fn new(config: ProviderConfig, client: Client, api_key: Option<String>) -> Result<Self> {
        validate_provider(&config)?;
        Ok(Self {
            config,
            client,
            api_key,
        })
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        match self.api_key.as_deref() {
            Some(key) => Ok(request.bearer_auth(key)),
            None => Ok(request),
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatible {
    fn identity(&self) -> &str {
        &self.config.name
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>> {
        let secret = self.api_key.as_deref();
        let request = self.client.get(endpoint(&self.config.base_url, "models")?);
        let response = self
            .auth(request)?
            .send()
            .await
            .map_err(|error| client_error(error, secret))?;
        let status = response.status();
        let body = read_response_body(response, MAX_RESPONSE_BODY_BYTES, secret).await?;
        if !status.is_success() {
            bail!(
                "Provider '{}' model discovery failed (HTTP {}){}: {}",
                self.config.name,
                status,
                rate_limit_guidance(status),
                redact(&body, secret)
            );
        }
        let value: Value = serde_json::from_str(&body).context("Invalid model-list response")?;
        let models = value["data"]
            .as_array()
            .context("Model-list response is missing data array")?
            .iter()
            .filter_map(|model| model["id"].as_str())
            .map(|id| ModelInfo {
                id: id.to_owned(),
                display_name: None,
                capabilities: ModelCapabilities {
                    streaming: true,
                    tools: None,
                    structured_output: None,
                },
            })
            .collect();
        Ok(models)
    }

    async fn check(&self) -> Result<()> {
        self.discover_models().await.map(|_| ())
    }

    async fn generate(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ChatResponse> {
        if request.model.trim().is_empty() {
            bail!("Model identifier must not be empty");
        }
        let secret = self.api_key.as_deref();
        let mut payload = json!({
            "model": request.model,
            "messages": request_messages(&request),
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        if !request.tools.is_empty() {
            payload["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters
                            }
                        })
                    })
                    .collect(),
            );
        }
        if let Some(schema) = request.json_schema {
            payload["response_format"] = json!({
                "type": "json_schema",
                "json_schema": schema
            });
        }
        let request_builder = self
            .client
            .post(endpoint(&self.config.base_url, "chat/completions")?)
            .json(&payload);
        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            result = self.auth(request_builder)?.send() => {
                result.map_err(|error| client_error(error, secret))?
            }
        };
        read_openai_stream(
            response,
            &self.config.name,
            &request.model,
            secret,
            cancel,
            on_delta,
        )
        .await
    }
}

struct Anthropic {
    config: ProviderConfig,
    client: Client,
    api_key: String,
}

impl Anthropic {
    fn new(config: ProviderConfig, client: Client, api_key: String) -> Result<Self> {
        validate_provider(&config)?;
        Ok(Self {
            config,
            client,
            api_key,
        })
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
    }
}

#[async_trait]
impl ModelProvider for Anthropic {
    fn identity(&self) -> &str {
        &self.config.name
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .auth(
                self.client
                    .get(endpoint(&self.config.base_url, "v1/models")?),
            )
            .send()
            .await
            .map_err(|error| client_error(error, Some(&self.api_key)))?;
        let status = response.status();
        let body =
            read_response_body(response, MAX_RESPONSE_BODY_BYTES, Some(&self.api_key)).await?;
        if !status.is_success() {
            bail!(
                "Anthropic model discovery failed (HTTP {status}){}: {}",
                rate_limit_guidance(status),
                redact(&body, Some(&self.api_key))
            );
        }
        let value: Value =
            serde_json::from_str(&body).context("Invalid Anthropic model-list response")?;
        let models = value["data"]
            .as_array()
            .context("Anthropic model-list response is missing data array")?
            .iter()
            .filter_map(|model| {
                let id = model["id"].as_str()?;
                Some(ModelInfo {
                    id: id.to_owned(),
                    display_name: model["display_name"].as_str().map(str::to_owned),
                    capabilities: ModelCapabilities {
                        streaming: true,
                        tools: Some(true),
                        structured_output: Some(false),
                    },
                })
            })
            .collect();
        Ok(models)
    }

    async fn check(&self) -> Result<()> {
        self.discover_models().await.map(|_| ())
    }

    async fn generate(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ChatResponse> {
        if request.model.trim().is_empty() {
            bail!("Model identifier must not be empty");
        }
        if request.json_schema.is_some() {
            bail!("Anthropic structured output is not implemented");
        }
        let mut messages = request
            .messages
            .iter()
            .filter(|message| message.role != ChatRole::System)
            .map(|message| {
                json!({
                    "role": match message.role {
                        ChatRole::Assistant => "assistant",
                        ChatRole::User | ChatRole::System => "user",
                    },
                    "content": message.content
                })
            })
            .collect::<Vec<_>>();
        if messages.is_empty() {
            messages.push(json!({"role": "user", "content": request.prompt}));
        }
        let system = request
            .messages
            .iter()
            .filter(|message| message.role == ChatRole::System)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut payload = json!({
            "model": request.model,
            "max_tokens": 4096,
            "messages": messages,
            "stream": true
        });
        if !system.is_empty() {
            payload["system"] = json!(system);
        }
        if !request.tools.is_empty() {
            payload["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.parameters
                        })
                    })
                    .collect(),
            );
        }
        let builder = self
            .client
            .post(endpoint(&self.config.base_url, "v1/messages")?)
            .json(&payload);
        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            result = self.auth(builder).send() => {
                result.map_err(|error| client_error(error, Some(&self.api_key)))?
            }
        };
        read_anthropic_stream(
            response,
            &self.config.name,
            &request.model,
            &self.api_key,
            cancel,
            on_delta,
        )
        .await
    }
}

struct Gemini {
    config: ProviderConfig,
    client: Client,
    api_key: String,
}

impl Gemini {
    fn new(config: ProviderConfig, client: Client, api_key: String) -> Result<Self> {
        validate_provider(&config)?;
        Ok(Self {
            config,
            client,
            api_key,
        })
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header("x-goog-api-key", &self.api_key)
    }
}

#[async_trait]
impl ModelProvider for Gemini {
    fn identity(&self) -> &str {
        &self.config.name
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .auth(self.client.get(endpoint(&self.config.base_url, "models")?))
            .send()
            .await
            .map_err(|error| client_error(error, Some(&self.api_key)))?;
        let status = response.status();
        let body =
            read_response_body(response, MAX_RESPONSE_BODY_BYTES, Some(&self.api_key)).await?;
        if !status.is_success() {
            bail!(
                "Google Gemini model discovery failed (HTTP {status}){}: {}",
                rate_limit_guidance(status),
                redact(&body, Some(&self.api_key))
            );
        }
        let value: Value =
            serde_json::from_str(&body).context("Invalid Google Gemini model-list response")?;
        let models = value["models"]
            .as_array()
            .context("Google Gemini model-list response is missing models array")?
            .iter()
            .filter(|model| {
                model["supportedGenerationMethods"]
                    .as_array()
                    .is_some_and(|methods| {
                        methods.iter().any(|method| {
                            matches!(
                                method.as_str(),
                                Some("generateContent" | "streamGenerateContent")
                            )
                        })
                    })
            })
            .filter_map(|model| {
                let id = model["name"].as_str()?.strip_prefix("models/")?;
                Some(ModelInfo {
                    id: id.to_owned(),
                    display_name: model["displayName"].as_str().map(str::to_owned),
                    capabilities: ModelCapabilities {
                        streaming: true,
                        tools: Some(true),
                        structured_output: Some(false),
                    },
                })
            })
            .collect();
        Ok(models)
    }

    async fn check(&self) -> Result<()> {
        self.discover_models().await.map(|_| ())
    }

    async fn generate(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ChatResponse> {
        if request.model.trim().is_empty() {
            bail!("Model identifier must not be empty");
        }
        if request.json_schema.is_some() {
            bail!("Google Gemini structured output is not implemented");
        }
        let mut contents = request
            .messages
            .iter()
            .filter(|message| message.role != ChatRole::System)
            .map(|message| {
                json!({
                    "role": if message.role == ChatRole::Assistant { "model" } else { "user" },
                    "parts": [{"text": message.content}]
                })
            })
            .collect::<Vec<_>>();
        if contents.is_empty() {
            contents.push(json!({"role": "user", "parts": [{"text": request.prompt}]}));
        }
        let mut payload = json!({"contents": contents});
        let system = request
            .messages
            .iter()
            .filter(|message| message.role == ChatRole::System)
            .map(|message| json!({"text": message.content}))
            .collect::<Vec<_>>();
        if !system.is_empty() {
            payload["systemInstruction"] = json!({"parts": system});
        }
        if !request.tools.is_empty() {
            payload["tools"] = json!([{
                "functionDeclarations": request.tools.iter().map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters
                })).collect::<Vec<_>>()
            }]);
        }
        let model = request
            .model
            .strip_prefix("models/")
            .unwrap_or(&request.model);
        let mut url =
            Url::parse(&self.config.base_url).context("Invalid Google Gemini endpoint")?;
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                anyhow::anyhow!("Google Gemini endpoint cannot accept a model path")
            })?;
            segments.pop_if_empty();
            segments.push("models");
            segments.push(&format!("{model}:streamGenerateContent"));
        }
        url.query_pairs_mut().append_pair("alt", "sse");
        let builder = self.client.post(url).json(&payload);
        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            result = self.auth(builder).send() => {
                result.map_err(|error| client_error(error, Some(&self.api_key)))?
            }
        };
        read_gemini_stream(
            response,
            &self.config.name,
            &request.model,
            &self.api_key,
            cancel,
            on_delta,
        )
        .await
    }
}

struct Ollama {
    config: ProviderConfig,
    client: Client,
}

impl Ollama {
    fn new(config: ProviderConfig, client: Client) -> Result<Self> {
        validate_provider(&config)?;
        Ok(Self { config, client })
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        let mut base = Url::parse(&self.config.base_url).context("Invalid Ollama endpoint")?;
        if base
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("localhost"))
        {
            base.set_host(Some("127.0.0.1"))
                .context("Could not normalize Ollama loopback endpoint")?;
        }
        endpoint(base.as_str(), path)
    }
}

#[async_trait]
impl ModelProvider for Ollama {
    fn identity(&self) -> &str {
        &self.config.name
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(self.endpoint("api/tags")?)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    anyhow::anyhow!("Ollama provider '{}' request timed out", self.config.name)
                } else {
                    anyhow::anyhow!(
                        "Could not connect to Ollama provider '{}' at {}: {}",
                        self.config.name,
                        self.config.base_url,
                        error
                    )
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            bail!("Ollama model discovery failed (HTTP {status})");
        }
        #[derive(Deserialize)]
        struct Tags {
            models: Vec<TagModel>,
        }
        #[derive(Deserialize)]
        struct TagModel {
            name: String,
            #[serde(default)]
            capabilities: Vec<String>,
            #[serde(default)]
            details: Option<TagDetails>,
        }
        #[derive(Deserialize)]
        struct TagDetails {
            family: Option<String>,
        }
        let body = read_response_body(response, MAX_RESPONSE_BODY_BYTES, None).await?;
        let tags: Tags =
            serde_json::from_str(&body).context("Invalid Ollama model-list response")?;
        Ok(tags
            .models
            .into_iter()
            .map(|model| {
                let supports_tools = model
                    .capabilities
                    .iter()
                    .any(|capability| capability == "tools");
                ModelInfo {
                    id: model.name,
                    display_name: model.details.and_then(|details| details.family),
                    capabilities: ModelCapabilities {
                        streaming: true,
                        tools: Some(supports_tools),
                        structured_output: Some(true),
                    },
                }
            })
            .collect())
    }

    async fn check(&self) -> Result<()> {
        self.discover_models()
            .await
            .map_err(|error| anyhow::anyhow!("Ollama unavailable: {error}"))
            .map(|_| ())
    }

    async fn generate(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ChatResponse> {
        if request.model.trim().is_empty() {
            bail!("Model identifier must not be empty");
        }
        let mut payload = json!({
            "model": request.model,
            "messages": request_messages(&request),
            "stream": true
        });
        if !request.tools.is_empty() {
            payload["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters
                            }
                        })
                    })
                    .collect(),
            );
        }
        if let Some(schema) = request.json_schema {
            payload["format"] = schema;
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            result = self.client
                .post(self.endpoint("api/chat")?)
                .json(&payload)
                .send() => {
                result.map_err(|error| {
                    if error.is_timeout() {
                        anyhow::anyhow!("Ollama request timed out")
                    } else {
                        anyhow::anyhow!(
                            "Could not connect to Ollama at {}: {}",
                            self.config.base_url,
                            error
                        )
                    }
                })?
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let body = read_response_body(response, MAX_ERROR_BODY_BYTES, None).await?;
            bail!("Ollama request failed (HTTP {status}): {body}");
        }
        read_ollama_stream(
            response,
            &self.config.name,
            &request.model,
            cancel,
            on_delta,
        )
        .await
    }
}

fn request_messages(request: &ChatRequest) -> Vec<Value> {
    if request.messages.is_empty() {
        return vec![json!({"role": "user", "content": request.prompt})];
    }
    request
        .messages
        .iter()
        .map(|message| json!({"role": message.role.as_str(), "content": message.content}))
        .collect()
}

async fn read_anthropic_stream(
    response: reqwest::Response,
    provider: &str,
    model: &str,
    secret: &str,
    cancel: CancellationToken,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<ChatResponse> {
    let status = response.status();
    if !status.is_success() {
        let body = read_response_body(response, MAX_ERROR_BODY_BYTES, Some(secret)).await?;
        bail!(
            "Provider '{provider}' request failed (HTTP {status}){}: {}",
            rate_limit_guidance(status),
            redact(&body, Some(secret))
        );
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut content = String::new();
    let mut usage = Usage::default();
    let mut tool_calls: Vec<(usize, ToolCallRequest)> = Vec::new();
    let mut done = false;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            next = stream.next() => next,
        };
        let Some(chunk) = chunk else { break };
        buffer.extend_from_slice(&chunk.map_err(|error| client_error(error, Some(secret)))?);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            if end > MAX_STREAM_EVENT_BYTES {
                bail!("Provider '{provider}' sent an oversized streaming event");
            }
            let line = String::from_utf8_lossy(&buffer[..end]);
            if let Some(data) = line.trim().strip_prefix("data:") {
                done = process_anthropic_event(
                    data.trim_start(),
                    &mut content,
                    &mut usage,
                    &mut tool_calls,
                    secret,
                    on_delta,
                )?;
            }
            buffer.drain(..=end);
            if done {
                break;
            }
        }
        if done {
            break;
        }
        if buffer.len() > MAX_STREAM_EVENT_BYTES {
            bail!("Provider '{provider}' sent an oversized streaming event");
        }
    }
    if !done && !buffer.is_empty() {
        let line = String::from_utf8_lossy(&buffer);
        if let Some(data) = line.trim().strip_prefix("data:") {
            done = process_anthropic_event(
                data.trim_start(),
                &mut content,
                &mut usage,
                &mut tool_calls,
                secret,
                on_delta,
            )?;
        }
    }
    if !done {
        bail!("Provider '{provider}' stream ended before its message_stop event");
    }
    let mut completed_calls = Vec::with_capacity(tool_calls.len());
    for (_, mut call) in tool_calls {
        if let Value::String(arguments) = &call.arguments {
            call.arguments = serde_json::from_str(arguments)
                .context("Anthropic returned invalid streamed tool-call JSON")?;
        }
        completed_calls.push(call);
    }
    Ok(ChatResponse {
        provider: provider.to_owned(),
        model: model.to_owned(),
        content,
        usage,
        tool_calls: completed_calls,
    })
}

fn process_anthropic_event(
    data: &str,
    content: &mut String,
    usage: &mut Usage,
    tool_calls: &mut Vec<(usize, ToolCallRequest)>,
    secret: &str,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<bool> {
    if data.is_empty() {
        return Ok(false);
    }
    let event: Value = serde_json::from_str(data).context("Invalid Anthropic streaming event")?;
    if event["type"] == "error" {
        bail!(
            "Anthropic returned a streaming error: {}",
            redact(&event["error"].to_string(), Some(secret))
        );
    }
    if let Some(tokens) = event["message"]["usage"]["input_tokens"].as_u64() {
        usage.input_tokens = Some(tokens);
    }
    if let Some(tokens) = event["usage"]["output_tokens"].as_u64() {
        usage.output_tokens = Some(tokens);
    }
    if event["type"] == "content_block_start"
        && event["content_block"]["type"] == "tool_use"
        && let (Some(index), Some(name)) = (
            event["index"].as_u64(),
            event["content_block"]["name"].as_str(),
        )
    {
        tool_calls.push((
            index as usize,
            ToolCallRequest {
                id: event["content_block"]["id"].as_str().map(str::to_owned),
                name: name.to_owned(),
                arguments: event["content_block"]["input"].clone(),
            },
        ));
    }
    if event["type"] == "content_block_delta" {
        if let Some(delta) = event["delta"]["text"].as_str() {
            append_generated_text(content, delta)?;
            on_delta(delta);
        }
        if let Some(fragment) = event["delta"]["partial_json"].as_str()
            && let Some(index) = event["index"].as_u64()
            && let Some((_, call)) = tool_calls
                .iter_mut()
                .find(|(slot, _)| *slot == index as usize)
        {
            let previous = call.arguments.as_str().unwrap_or_default();
            call.arguments = Value::String(format!("{previous}{fragment}"));
        }
    }
    Ok(event["type"] == "message_stop")
}

async fn read_gemini_stream(
    response: reqwest::Response,
    provider: &str,
    model: &str,
    secret: &str,
    cancel: CancellationToken,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<ChatResponse> {
    let status = response.status();
    if !status.is_success() {
        let body = read_response_body(response, MAX_ERROR_BODY_BYTES, Some(secret)).await?;
        bail!(
            "Provider '{provider}' request failed (HTTP {status}){}: {}",
            rate_limit_guidance(status),
            redact(&body, Some(secret))
        );
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut content = String::new();
    let mut usage = Usage::default();
    let mut tool_calls = Vec::new();
    let mut saw_event = false;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            next = stream.next() => next,
        };
        let Some(chunk) = chunk else { break };
        buffer.extend_from_slice(&chunk.map_err(|error| client_error(error, Some(secret)))?);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            if end > MAX_STREAM_EVENT_BYTES {
                bail!("Provider '{provider}' sent an oversized streaming event");
            }
            let line = String::from_utf8_lossy(&buffer[..end]);
            if let Some(data) = line.trim().strip_prefix("data:") {
                saw_event |= process_gemini_event(
                    data.trim_start(),
                    &mut content,
                    &mut usage,
                    &mut tool_calls,
                    secret,
                    on_delta,
                )?;
            }
            buffer.drain(..=end);
        }
        if buffer.len() > MAX_STREAM_EVENT_BYTES {
            bail!("Provider '{provider}' sent an oversized streaming event");
        }
    }
    if !buffer.is_empty() {
        let line = String::from_utf8_lossy(&buffer);
        if let Some(data) = line.trim().strip_prefix("data:") {
            saw_event |= process_gemini_event(
                data.trim_start(),
                &mut content,
                &mut usage,
                &mut tool_calls,
                secret,
                on_delta,
            )?;
        }
    }
    if !saw_event {
        bail!("Provider '{provider}' stream ended without a response event");
    }
    Ok(ChatResponse {
        provider: provider.to_owned(),
        model: model.to_owned(),
        content,
        usage,
        tool_calls,
    })
}

fn process_gemini_event(
    data: &str,
    content: &mut String,
    usage: &mut Usage,
    tool_calls: &mut Vec<ToolCallRequest>,
    secret: &str,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<bool> {
    if data.is_empty() {
        return Ok(false);
    }
    let event: Value =
        serde_json::from_str(data).context("Invalid Google Gemini streaming event")?;
    if !event["error"].is_null() {
        bail!(
            "Google Gemini returned a streaming error: {}",
            redact(&event["error"].to_string(), Some(secret))
        );
    }
    if let Some(tokens) = event["usageMetadata"]["promptTokenCount"].as_u64() {
        usage.input_tokens = Some(tokens);
    }
    if let Some(tokens) = event["usageMetadata"]["candidatesTokenCount"].as_u64() {
        usage.output_tokens = Some(tokens);
    }
    if let Some(candidates) = event["candidates"].as_array() {
        for candidate in candidates {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if let Some(delta) = part["text"].as_str() {
                        append_generated_text(content, delta)?;
                        on_delta(delta);
                    }
                    if let Some(name) = part["functionCall"]["name"].as_str() {
                        tool_calls.push(ToolCallRequest {
                            id: None,
                            name: name.to_owned(),
                            arguments: part["functionCall"]["args"].clone(),
                        });
                    }
                }
            }
        }
        return Ok(true);
    }
    Ok(false)
}

fn rate_limit_guidance(status: reqwest::StatusCode) -> &'static str {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        "; the provider rate limit was reached; check quota or retry later"
    } else {
        ""
    }
}

async fn read_openai_stream(
    response: reqwest::Response,
    provider: &str,
    model: &str,
    secret: Option<&str>,
    cancel: CancellationToken,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<ChatResponse> {
    let status = response.status();
    if !status.is_success() {
        let body = read_response_body(response, MAX_ERROR_BODY_BYTES, secret).await?;
        bail!(
            "Provider '{provider}' request failed (HTTP {status}){}: {}",
            rate_limit_guidance(status),
            redact(&body, secret)
        );
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut content = String::new();
    let mut usage = Usage::default();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    let mut done = false;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            next = stream.next() => next,
        };
        let Some(chunk) = chunk else { break };
        buffer.extend_from_slice(&chunk.map_err(|error| client_error(error, secret))?);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            if end > MAX_STREAM_EVENT_BYTES {
                bail!("Provider '{provider}' sent an oversized streaming event");
            }
            let line = String::from_utf8(buffer[..end].to_vec())
                .context("Provider sent a non-UTF-8 SSE event")?;
            buffer.drain(..=end);
            done = process_openai_line(
                &line,
                &mut content,
                &mut usage,
                &mut tool_calls,
                secret,
                on_delta,
            )?;
            if done {
                break;
            }
        }
        if done {
            break;
        }
        if buffer.len() > MAX_STREAM_EVENT_BYTES {
            bail!("Provider '{provider}' sent an oversized streaming event");
        }
    }
    if !done && !buffer.is_empty() {
        let line =
            String::from_utf8(buffer).context("Provider sent a non-UTF-8 final SSE event")?;
        done = process_openai_line(
            &line,
            &mut content,
            &mut usage,
            &mut tool_calls,
            secret,
            on_delta,
        )?;
    }
    if !done {
        bail!("Provider '{provider}' stream ended before its [DONE] marker");
    }
    for call in &mut tool_calls {
        if let Value::String(arguments) = &call.arguments {
            call.arguments = serde_json::from_str(arguments)
                .context("Provider returned invalid streamed tool-call JSON")?;
        }
    }
    Ok(ChatResponse {
        provider: provider.to_owned(),
        model: model.to_owned(),
        content,
        usage,
        tool_calls,
    })
}

fn process_openai_line(
    line: &str,
    content: &mut String,
    usage: &mut Usage,
    tool_calls: &mut Vec<ToolCallRequest>,
    secret: Option<&str>,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<bool> {
    let Some(data) = line.trim().strip_prefix("data:") else {
        return Ok(false);
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        return Ok(true);
    }
    if data.is_empty() {
        return Ok(false);
    }
    let event: Value =
        serde_json::from_str(data).context("Invalid OpenAI-compatible streaming event")?;
    if !event["error"].is_null() {
        bail!(
            "Provider returned a streaming error: {}",
            redact(&event["error"].to_string(), secret)
        );
    }
    if let Some(delta) = event["choices"][0]["delta"]["content"].as_str() {
        append_generated_text(content, delta)?;
        on_delta(delta);
    }
    if let Some(calls) = event["choices"][0]["delta"]["tool_calls"].as_array() {
        for call in calls {
            let raw_index = call["index"]
                .as_u64()
                .context("Provider returned a tool call without a valid index")?;
            let index = usize::try_from(raw_index)
                .context("Provider returned a tool call index outside the supported range")?;
            if index >= MAX_STREAM_TOOL_CALLS {
                bail!(
                    "Provider returned a tool call index beyond the {}-call stream limit",
                    MAX_STREAM_TOOL_CALLS
                );
            }
            if tool_calls.len() <= index {
                tool_calls.resize_with(index + 1, || ToolCallRequest {
                    id: None,
                    name: String::new(),
                    arguments: Value::String(String::new()),
                });
            }
            let target = &mut tool_calls[index];
            if let Some(id) = call["id"].as_str() {
                target.id = Some(id.to_owned());
            }
            if let Some(name) = call["function"]["name"].as_str() {
                target.name.push_str(name);
            }
            if let Some(fragment) = call["function"]["arguments"].as_str() {
                let previous = target.arguments.as_str().unwrap_or_default();
                target.arguments = Value::String(format!("{previous}{fragment}"));
            }
        }
    }
    if let Some(value) = event["usage"]["prompt_tokens"].as_u64() {
        usage.input_tokens = Some(value);
    }
    if let Some(value) = event["usage"]["completion_tokens"].as_u64() {
        usage.output_tokens = Some(value);
    }
    Ok(false)
}

fn append_generated_text(content: &mut String, delta: &str) -> Result<()> {
    if content.len().saturating_add(delta.len()) > MAX_GENERATED_TEXT_BYTES {
        bail!(
            "Provider response exceeded the {} byte output limit",
            MAX_GENERATED_TEXT_BYTES
        );
    }
    content.push_str(delta);
    Ok(())
}

async fn read_response_body(
    response: reqwest::Response,
    max_bytes: usize,
    secret: Option<&str>,
) -> Result<String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| client_error(error, secret))?;
        let available = max_bytes.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(available)]);
        if chunk.len() > available {
            truncated = true;
            break;
        }
    }
    let mut text = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        text.push_str("\n[response truncated]");
    }
    Ok(redact(&text, secret))
}

async fn read_ollama_stream(
    response: reqwest::Response,
    provider: &str,
    requested_model: &str,
    cancel: CancellationToken,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<ChatResponse> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut content = String::new();
    let mut model = requested_model.to_owned();
    let mut usage = Usage::default();
    let mut tool_calls = Vec::new();
    let mut done = false;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => bail!("Request cancelled"),
            next = stream.next() => next,
        };
        let Some(chunk) = chunk else { break };
        buffer.extend_from_slice(&chunk.context("Ollama stream failed")?);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            if end > MAX_STREAM_EVENT_BYTES {
                bail!("Ollama sent an oversized streaming event");
            }
            let line = String::from_utf8(buffer[..end].to_vec())
                .context("Ollama sent a non-UTF-8 stream event")?;
            buffer.drain(..=end);
            done = process_ollama_line(
                &line,
                &mut model,
                &mut content,
                &mut usage,
                &mut tool_calls,
                on_delta,
            )?;
            if done {
                break;
            }
        }
        if done {
            break;
        }
        if buffer.len() > MAX_STREAM_EVENT_BYTES {
            bail!("Ollama sent an oversized streaming event");
        }
    }
    if !done && !buffer.is_empty() {
        let line = String::from_utf8(buffer).context("Ollama sent a non-UTF-8 final event")?;
        done = process_ollama_line(
            &line,
            &mut model,
            &mut content,
            &mut usage,
            &mut tool_calls,
            on_delta,
        )?;
    }
    if !done {
        bail!("Ollama stream ended before its final done event");
    }
    Ok(ChatResponse {
        provider: provider.to_owned(),
        model,
        content,
        usage,
        tool_calls,
    })
}

fn process_ollama_line(
    line: &str,
    model: &mut String,
    content: &mut String,
    usage: &mut Usage,
    tool_calls: &mut Vec<ToolCallRequest>,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<bool> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(false);
    }
    let event: Value = serde_json::from_str(line).context("Invalid Ollama streaming event")?;
    if let Some(error) = event["error"].as_str() {
        bail!("Ollama returned an error: {error}");
    }
    if let Some(name) = event["model"].as_str() {
        *model = name.to_owned();
    }
    if let Some(delta) = event["message"]["content"].as_str() {
        append_generated_text(content, delta)?;
        on_delta(delta);
    }
    if let Some(calls) = event["message"]["tool_calls"].as_array() {
        for call in calls {
            let function = &call["function"];
            tool_calls.push(ToolCallRequest {
                id: None,
                name: function["name"].as_str().unwrap_or_default().to_owned(),
                arguments: function["arguments"].clone(),
            });
        }
    }
    usage.input_tokens = event["prompt_eval_count"].as_u64();
    usage.output_tokens = event["eval_count"].as_u64();
    Ok(event["done"].as_bool() == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, ModelConfig};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc::{self, Receiver},
        thread,
    };

    fn config(provider: ProviderConfig) -> AppConfig {
        AppConfig {
            database_path: std::env::temp_dir().join("venus-provider-test.sqlite3"),
            log_level: "off".to_owned(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: vec![provider],
            models: vec![ModelConfig {
                name: "test-model".into(),
                provider: "test".into(),
                model_id: "custom/model-id".into(),
                task: Some("coding".into()),
            }],
            config_path: std::env::temp_dir().join("venus-test-config.toml"),
        }
    }

    #[test]
    fn request_messages_preserve_conversation_roles() {
        let request = ChatRequest {
            model: "model".to_owned(),
            prompt: "current question".to_owned(),
            messages: vec![
                ChatMessage {
                    role: ChatRole::User,
                    content: "Earlier question".to_owned(),
                },
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: "Earlier answer".to_owned(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "Current question".to_owned(),
                },
            ],
            json_schema: None,
            tools: Vec::new(),
        };
        assert_eq!(
            request_messages(&request),
            vec![
                json!({"role": "user", "content": "Earlier question"}),
                json!({"role": "assistant", "content": "Earlier answer"}),
                json!({"role": "user", "content": "Current question"}),
            ]
        );
    }

    #[test]
    fn request_messages_keep_legacy_single_prompt_behavior() {
        let request = ChatRequest {
            model: "model".to_owned(),
            prompt: "hello".to_owned(),
            messages: Vec::new(),
            json_schema: None,
            tools: Vec::new(),
        };
        assert_eq!(
            request_messages(&request),
            vec![json!({"role": "user", "content": "hello"})]
        );
    }

    #[test]
    fn rejects_openai_tool_call_indices_outside_the_stream_limit() {
        let mut content = String::new();
        let mut usage = Usage::default();
        let mut tool_calls = Vec::new();
        let mut callback = |_delta: &str| {};
        let line = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":{}}}]}}}}]}}",
            u64::MAX
        );

        let error = process_openai_line(
            &line,
            &mut content,
            &mut usage,
            &mut tool_calls,
            None,
            &mut callback,
        )
        .expect_err("oversized tool-call index must be rejected");

        assert!(error.to_string().contains("stream limit"));
        assert!(tool_calls.is_empty());
    }

    fn mock_server(status: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0; 8192];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        format!("http://{address}/v1")
    }

    fn mock_capture_server(body: &'static str) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let count = stream.read(&mut chunk).expect("read request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            sender
                .send(String::from_utf8_lossy(&request).into_owned())
                .expect("send captured request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        (format!("http://{address}"), receiver)
    }

    fn streaming_server_that_stays_open() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0; 8192];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
                )
                .expect("write headers");
            let event = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
            write!(stream, "{:X}\r\n{event}\r\n", event.len()).expect("write event chunk");
            stream.flush().expect("flush event chunk");
            thread::sleep(Duration::from_secs(2));
        });
        format!("http://{address}/v1")
    }

    fn chunked_server(chunks: Vec<&'static [u8]>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0; 8192];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .expect("write response headers");
            for chunk in chunks {
                write!(stream, "{:X}\r\n", chunk.len()).expect("write chunk length");
                stream.write_all(chunk).expect("write chunk data");
                stream.write_all(b"\r\n").expect("write chunk terminator");
            }
            stream
                .write_all(b"0\r\n\r\n")
                .expect("finish chunked response");
        });
        format!("http://{address}/v1")
    }

    fn provider(base_url: String, kind: ProviderKind) -> ProviderConfig {
        ProviderConfig {
            name: "test".into(),
            kind,
            base_url,
            api_key_env: None,
            timeout_secs: 3,
        }
    }

    #[tokio::test]
    async fn openai_compatible_streaming_returns_content_and_usage() -> Result<()> {
        let url = mock_server(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}]}}]}\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\
             data: [DONE]\n",
        );
        let config = config(provider(url, ProviderKind::OpenaiCompatible));
        let selected = create_provider(&config, "test")?;
        let mut output = String::new();
        let mut callback = |delta: &str| output.push_str(delta);
        let response = selected
            .generate(
                ChatRequest {
                    model: "custom/model-id".into(),
                    prompt: "hi".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                CancellationToken::new(),
                &mut callback,
            )
            .await?;
        assert_eq!(response.content, "hello world");
        assert_eq!(output, response.content);
        assert_eq!(response.usage.input_tokens, Some(3));
        assert_eq!(response.model, "custom/model-id");
        assert_eq!(response.tool_calls[0].name, "lookup");
        assert_eq!(response.tool_calls[0].arguments["q"], "x");
        Ok(())
    }

    #[tokio::test]
    async fn streaming_parser_preserves_utf8_split_across_http_chunks() -> Result<()> {
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"café 🪷\"}}]}\n\n";
        let bytes = event.as_bytes();
        let lotus = "🪷".as_bytes();
        let split = bytes
            .windows(lotus.len())
            .position(|window| window == lotus)
            .context("test event contains lotus character")?
            + 2;
        let url = chunked_server(vec![&bytes[..split], &bytes[split..], b"data: [DONE]\n\n"]);
        let selected = create_provider(
            &config(provider(url, ProviderKind::OpenaiCompatible)),
            "test",
        )?;
        let response = selected
            .generate(
                ChatRequest {
                    model: "model".into(),
                    prompt: "prompt".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                CancellationToken::new(),
                &mut |_| {},
            )
            .await?;
        assert_eq!(response.content, "café 🪷");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_truncated_stream_instead_of_reporting_partial_success() -> Result<()> {
        let url = mock_server(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n",
        );
        let selected = create_provider(
            &config(provider(url, ProviderKind::OpenaiCompatible)),
            "test",
        )?;
        let error = selected
            .generate(
                ChatRequest {
                    model: "model".into(),
                    prompt: "prompt".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .expect_err("missing done marker is incomplete");
        assert!(error.to_string().contains("[DONE]"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_malformed_stream_events() -> Result<()> {
        let url = mock_server("200 OK", "data: {not-json}\ndata: [DONE]\n");
        let selected = create_provider(
            &config(provider(url, ProviderKind::OpenaiCompatible)),
            "test",
        )?;
        let error = selected
            .generate(
                ChatRequest {
                    model: "model".into(),
                    prompt: "prompt".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .expect_err("malformed event must fail");
        assert!(error.to_string().contains("Invalid OpenAI-compatible"));
        Ok(())
    }

    #[tokio::test]
    async fn reports_errors_sent_inside_successful_sse_streams() -> Result<()> {
        let url = mock_server(
            "200 OK",
            "data: {\"error\":{\"message\":\"model unavailable\"}}\ndata: [DONE]\n",
        );
        let selected = create_provider(
            &config(provider(url, ProviderKind::OpenaiCompatible)),
            "test",
        )?;
        let error = selected
            .generate(
                ChatRequest {
                    model: "model".into(),
                    prompt: "prompt".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .expect_err("SSE error payload must fail");
        assert!(error.to_string().contains("model unavailable"));
        Ok(())
    }

    #[tokio::test]
    async fn ollama_discovers_local_models_without_credentials() -> Result<()> {
        let url = mock_server(
            "200 OK",
            r#"{"models":[{"name":"qwen2.5:7b","details":{"family":"qwen2"},"capabilities":["completion","tools"]}]}"#,
        )
        .replace("127.0.0.1", "localhost")
        .replace("/v1", "");
        let config = config(provider(url, ProviderKind::Ollama));
        let selected = create_provider(&config, "test")?;
        let models = selected.discover_models().await?;
        assert_eq!(models[0].id, "qwen2.5:7b");
        assert_eq!(models[0].capabilities.tools, Some(true));
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_uses_native_auth_messages_endpoint_and_streaming() -> Result<()> {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (url, requests) = mock_capture_server(body);
        let mut provider = provider(url, ProviderKind::Anthropic);
        provider.api_key_env = Some("ANTHROPIC_TEST_KEY".into());
        let selected = create_provider_with_environment(&config(provider), "test", |_| {
            Ok("anthropic-secret".to_owned())
        })?;
        let mut streamed = String::new();
        let mut callback = |delta: &str| streamed.push_str(delta);
        let response = selected
            .generate(
                ChatRequest {
                    model: "claude-test".into(),
                    prompt: "Say hi".into(),
                    messages: vec![
                        ChatMessage {
                            role: ChatRole::System,
                            content: "Be concise".into(),
                        },
                        ChatMessage {
                            role: ChatRole::User,
                            content: "Say hi".into(),
                        },
                    ],
                    json_schema: None,
                    tools: Vec::new(),
                },
                CancellationToken::new(),
                &mut callback,
            )
            .await?;
        let request = requests.recv_timeout(Duration::from_secs(2))?;
        assert!(request.starts_with("POST /v1/messages "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-api-key: anthropic-secret")
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("anthropic-version: 2023-06-01")
        );
        assert_eq!(response.content, "Hello");
        assert_eq!(streamed, "Hello");
        assert_eq!(response.usage.input_tokens, Some(5));
        assert_eq!(response.usage.output_tokens, Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_discovers_models_using_its_authenticated_api() -> Result<()> {
        let body = r#"{"data":[{"id":"claude-test","display_name":"Claude Test"}]}"#;
        let (url, requests) = mock_capture_server(body);
        let mut provider = provider(url, ProviderKind::Anthropic);
        provider.api_key_env = Some("ANTHROPIC_TEST_KEY".into());
        let selected = create_provider_with_environment(&config(provider), "test", |_| {
            Ok("anthropic-secret".to_owned())
        })?;
        let models = selected.discover_models().await?;
        let request = requests.recv_timeout(Duration::from_secs(2))?;
        assert!(request.starts_with("GET /v1/models "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-api-key: anthropic-secret")
        );
        assert_eq!(models[0].id, "claude-test");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Test"));
        Ok(())
    }

    #[tokio::test]
    async fn gemini_discovers_models_using_its_authenticated_api() -> Result<()> {
        let body = r#"{"models":[{"name":"models/gemini-test","displayName":"Gemini Test","supportedGenerationMethods":["generateContent"]}]}"#;
        let (url, requests) = mock_capture_server(body);
        let mut provider = provider(format!("{url}/v1beta"), ProviderKind::Gemini);
        provider.api_key_env = Some("GEMINI_TEST_KEY".into());
        let selected = create_provider_with_environment(&config(provider), "test", |_| {
            Ok("gemini-secret".to_owned())
        })?;
        let models = selected.discover_models().await?;
        let request = requests.recv_timeout(Duration::from_secs(2))?;
        assert!(request.starts_with("GET /v1beta/models "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-goog-api-key: gemini-secret")
        );
        assert_eq!(models[0].id, "gemini-test");
        assert_eq!(models[0].display_name.as_deref(), Some("Gemini Test"));
        Ok(())
    }

    #[tokio::test]
    async fn gemini_uses_native_auth_and_streaming() -> Result<()> {
        let body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":1}}\n\n";
        let (url, requests) = mock_capture_server(body);
        let mut provider = provider(format!("{url}/v1beta"), ProviderKind::Gemini);
        provider.api_key_env = Some("GEMINI_TEST_KEY".into());
        let selected = create_provider_with_environment(&config(provider), "test", |_| {
            Ok("gemini-secret".to_owned())
        })?;
        let mut streamed = String::new();
        let mut callback = |delta: &str| streamed.push_str(delta);
        let response = selected
            .generate(
                ChatRequest {
                    model: "gemini-test".into(),
                    prompt: "Say hi".into(),
                    messages: Vec::new(),
                    json_schema: None,
                    tools: Vec::new(),
                },
                CancellationToken::new(),
                &mut callback,
            )
            .await?;
        let request = requests.recv_timeout(Duration::from_secs(2))?;
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-goog-api-key: gemini-secret")
        );
        assert!(
            request.starts_with("POST /v1beta/models/gemini-test:streamGenerateContent?alt=sse "),
            "{request}"
        );
        assert_eq!(response.content, "Hello");
        assert_eq!(streamed, "Hello");
        assert_eq!(response.usage.input_tokens, Some(4));
        assert_eq!(response.usage.output_tokens, Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn ollama_uses_the_shared_streaming_chat_interface() -> Result<()> {
        let url = mock_server(
            "200 OK",
            "{\"model\":\"qwen2.5:7b\",\"message\":{\"role\":\"assistant\",\"content\":\"local reply\"},\"done\":false}\n\
             {\"model\":\"qwen2.5:7b\",\"done\":true,\"prompt_eval_count\":4,\"eval_count\":3}\n",
        );
        let config = config(provider(url, ProviderKind::Ollama));
        let selected = create_provider(&config, "test")?;
        let mut streamed = String::new();
        let response = selected
            .generate(
                ChatRequest {
                    model: "qwen2.5:7b".into(),
                    prompt: "hello".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                CancellationToken::new(),
                &mut |delta| streamed.push_str(delta),
            )
            .await?;
        assert_eq!(streamed, "local reply");
        assert_eq!(response.provider, "test");
        assert_eq!(response.usage.input_tokens, Some(4));
        Ok(())
    }

    #[test]
    fn provider_selection_switches_adapters_without_changing_the_contract() -> Result<()> {
        let compatible = create_provider(
            &config(provider(
                "http://127.0.0.1:8000/v1".into(),
                ProviderKind::OpenaiCompatible,
            )),
            "test",
        )?;
        let ollama = create_provider(
            &config(provider(
                "http://127.0.0.1:11434".into(),
                ProviderKind::Ollama,
            )),
            "test",
        )?;
        assert_eq!(compatible.identity(), "test");
        assert_eq!(ollama.identity(), "test");
        Ok(())
    }

    #[tokio::test]
    async fn reports_authentication_failure_without_echoing_secret() -> Result<()> {
        let url = mock_server(
            "401 Unauthorized",
            r#"{"error":"secret-for-test is invalid"}"#,
        );
        let mut provider = provider(url, ProviderKind::OpenaiCompatible);
        provider.api_key_env = Some("VENUS_PROVIDER_TEST_SECRET".into());
        let config = config(provider);
        let selected =
            create_provider_with_environment(
                &config,
                "test",
                |_| Ok("secret-for-test".to_owned()),
            )?;
        let error = selected
            .check()
            .await
            .expect_err("authentication must fail");
        let message = error.to_string();
        assert!(message.contains("401"));
        assert!(!message.contains("secret-for-test"));
        Ok(())
    }

    #[tokio::test]
    async fn all_cloud_adapters_redact_credentials_from_authentication_errors() -> Result<()> {
        for kind in [
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
        ] {
            let url = mock_server(
                "401 Unauthorized",
                r#"{"error":"secret-for-test is invalid"}"#,
            );
            let mut provider = provider(url, kind);
            provider.api_key_env = Some("VENUS_PROVIDER_TEST_SECRET".into());
            let selected = create_provider_with_environment(&config(provider), "test", |_| {
                Ok("secret-for-test".to_owned())
            })?;
            let error = selected
                .check()
                .await
                .expect_err("authentication must fail");
            assert!(error.to_string().contains("401"));
            assert!(!error.to_string().contains("secret-for-test"));
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_endpoint_and_missing_provider() {
        let config = config(provider(
            "file:///tmp/not-http".into(),
            ProviderKind::Ollama,
        ));
        assert!(create_provider(&config, "test").is_err());
        assert!(create_provider(&config, "missing").is_err());
    }

    #[test]
    fn provider_validation_allows_keyed_loopback_http_but_rejects_remote_http() {
        for endpoint in [
            "http://127.0.0.1:1234/v1",
            "http://localhost:1234/v1",
            "http://[::1]:1234/v1",
            "https://example.com/v1",
        ] {
            let mut configured = provider(endpoint.to_owned(), ProviderKind::OpenaiCompatible);
            configured.api_key_env = Some("LOCAL_API_KEY".into());
            validate_provider(&configured).expect(endpoint);
        }

        let mut remote = provider(
            "http://example.com/v1".into(),
            ProviderKind::OpenaiCompatible,
        );
        remote.api_key_env = Some("REMOTE_API_KEY".into());
        assert!(
            validate_provider(&remote)
                .expect_err("remote plaintext API-key endpoint must fail")
                .to_string()
                .contains("use HTTPS")
        );
    }

    #[tokio::test]
    async fn reports_unavailable_ollama_service() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        let config = config(provider(format!("http://{address}"), ProviderKind::Ollama));
        let selected = create_provider(&config, "test")?;
        let error = selected.check().await.expect_err("service is unavailable");
        assert!(error.to_string().contains("Ollama unavailable"));
        Ok(())
    }

    #[tokio::test]
    async fn applies_the_configured_request_timeout() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0; 8192];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_secs(2));
            let body = r#"{"data":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let mut settings = provider(
            format!("http://{address}/v1"),
            ProviderKind::OpenaiCompatible,
        );
        settings.timeout_secs = 1;
        let config = config(settings);
        let selected = create_provider(&config, "test")?;
        let error = selected.check().await.expect_err("request should time out");
        assert!(error.to_string().contains("timed out"));
        Ok(())
    }

    #[tokio::test]
    async fn reports_missing_environment_credential_before_request() -> Result<()> {
        let mut provider = provider(
            "http://127.0.0.1:1/v1".into(),
            ProviderKind::OpenaiCompatible,
        );
        provider.api_key_env = Some("VENUS_TEST_MISSING_CREDENTIAL".into());
        let config = config(provider);
        let error = create_provider_with_environment(&config, "test", |_| {
            Err(std::env::VarError::NotPresent)
        })
        .err()
        .context("credential is missing")?;
        assert!(error.to_string().contains("VENUS_TEST_MISSING_CREDENTIAL"));
        Ok(())
    }

    #[test]
    fn rejects_empty_environment_credentials() {
        let mut provider = provider(
            "http://127.0.0.1:1/v1".into(),
            ProviderKind::OpenaiCompatible,
        );
        provider.api_key_env = Some("VENUS_TEST_EMPTY_CREDENTIAL".into());
        let error =
            create_provider_with_environment(&config(provider), "test", |_| Ok(String::new()))
                .err()
                .expect("empty credentials must be rejected");
        assert!(error.to_string().contains("non-empty value"));
    }

    #[tokio::test]
    async fn cancellation_is_reported_without_waiting_for_network() -> Result<()> {
        let config = config(provider(
            "http://127.0.0.1:1/v1".into(),
            ProviderKind::OpenaiCompatible,
        ));
        let selected = create_provider(&config, "test")?;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = selected
            .generate(
                ChatRequest {
                    model: "model".into(),
                    prompt: "prompt".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                cancel,
                &mut |_| {},
            )
            .await
            .expect_err("request should cancel");
        assert!(error.to_string().contains("cancelled"));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_stops_an_active_stream() -> Result<()> {
        let config = config(provider(
            streaming_server_that_stays_open(),
            ProviderKind::OpenaiCompatible,
        ));
        let selected = create_provider(&config, "test")?;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (sent, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut sent = Some(sent);
            selected
                .generate(
                    ChatRequest {
                        model: "model".into(),
                        prompt: "prompt".into(),
                        messages: vec![],
                        json_schema: None,
                        tools: vec![],
                    },
                    task_cancel,
                    &mut |delta| {
                        if !delta.is_empty()
                            && let Some(sent) = sent.take()
                        {
                            let _ = sent.send(());
                        }
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .context("Stream did not produce its first chunk")??;
        cancel.cancel();
        let error = task
            .await
            .context("Provider task panicked")?
            .expect_err("active stream should be cancelled");
        assert!(error.to_string().contains("cancelled"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_empty_model_identifiers() -> Result<()> {
        let config = config(provider(
            "http://127.0.0.1:1/v1".into(),
            ProviderKind::OpenaiCompatible,
        ));
        let selected = create_provider(&config, "test")?;
        let error = selected
            .generate(
                ChatRequest {
                    model: " ".into(),
                    prompt: "prompt".into(),
                    messages: vec![],
                    json_schema: None,
                    tools: vec![],
                },
                CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .expect_err("empty model identifier must fail");
        assert!(error.to_string().contains("Model identifier"));
        Ok(())
    }
}
