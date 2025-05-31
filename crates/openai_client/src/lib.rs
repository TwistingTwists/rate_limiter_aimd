// src/openai_client.rs

use rate_limiter_aimd::adaptive_concurrency::Error as CrateError;
use rate_limiter_aimd::adaptive_concurrency::{
    AdaptiveConcurrencySettings,
    http::HttpError as GenericHttpError,
    layer::AdaptiveConcurrencyLimitLayer,
    reqwest_integration::ReqwestService,
    retries::{ExponentialBackoff, ExponentialBackoffPolicy, JitterMode, RetryAction, RetryLogic},
    service::AdaptiveConcurrencyLimit, // Ensure this is imported
};

use bytes::Bytes;
use http::{HeaderValue, Request as HttpRequest, StatusCode};
use reqwest::Response as ReqwestResponse;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, env, fmt, sync::Arc, time::Duration}; // Added env for CARGO_PKG_VERSION
use tower::{
    Service, ServiceBuilder, ServiceExt,
    retry::{Retry, RetryLayer},
}; // Added retry::Retry
use tracing::{debug, error, info, warn};

// --- Client Specific Error Type --- (remains the same)
#[derive(Debug)]
pub enum OpenAIClientError {
    Initialization(String),
    RequestBuild(String),
    Serialization(serde_json::Error),
    ServiceError,
    // ServiceError(CrateError),
    ApiError {
        status: u16,
        error_body: String,
    },
    ResponseDeserialization {
        status: u16,
        body_text: String,
        source: serde_json::Error,
    },
    StreamUnsupported,
}

impl fmt::Display for OpenAIClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenAIClientError::Initialization(s) => {
                write!(f, "OpenAI Client Initialization Error: {}", s)
            }
            OpenAIClientError::RequestBuild(s) => write!(f, "Failed to build request: {}", s),
            OpenAIClientError::Serialization(e) => write!(f, "JSON serialization error: {}", e),
            OpenAIClientError::ServiceError => write!(f, "Underlying service error"),
            // OpenAIClientError::ServiceError(e) => write!(f, "Underlying service error: {}", e),
            OpenAIClientError::ApiError { status, error_body } => {
                write!(f, "OpenAI API Error (status {}): {}", status, error_body)
            }
            OpenAIClientError::ResponseDeserialization {
                status,
                body_text,
                source,
            } => write!(
                f,
                "Failed to deserialize OpenAI response (status {}): {}. Body: {:.100}",
                status, source, body_text
            ),
            OpenAIClientError::StreamUnsupported => {
                write!(f, "Streaming is not supported by this method")
            }
        }
    }
}

impl std::error::Error for OpenAIClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenAIClientError::Serialization(e) => Some(e),
            OpenAIClientError::ServiceError => None,
            OpenAIClientError::ResponseDeserialization { source, .. } => Some(source),
            _ => None,
        }
    }
}

// --- Configuration for the Client --- (remains the same structure)
#[derive(Debug, Clone)]
pub struct OpenAIClientConfig {
    pub api_key: String,
    pub base_url: String,
    pub default_model: String,
    pub reqwest_client: Option<reqwest::Client>,
    pub adaptive_concurrency: AdaptiveConcurrencySettings,
    pub retry_max_attempts: usize,
    pub retry_initial_backoff_ms: u64,
    pub retry_exp_base: u64,
    pub retry_max_single_delay_secs: u64,
    pub user_agent: Option<String>,
}

impl Default for OpenAIClientConfig {
    fn default() -> Self {
        // Defaults similar to the robust example
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com".to_string(),
            default_model: "gpt-3.5-turbo".to_string(), // A common default
            reqwest_client: None,
            adaptive_concurrency: AdaptiveConcurrencySettings::builder()
                .initial_concurrency(2) // Start somewhat conservatively
                .max_concurrency_limit(8) // Max concurrency
                .decrease_ratio(0.75) // More aggressive decrease
                .rtt_deviation_scale(1.25) // More sensitive
                .ewma_alpha(0.3)
                .build(),
            retry_max_attempts: 10,          // As in the example
            retry_initial_backoff_ms: 1000,  // As in the example
            retry_exp_base: 2,               // As in the example
            retry_max_single_delay_secs: 60, // As in the example
            user_agent: Some(format!(
                "rust-openai-client-aimd/{}",
                option_env!("CARGO_PKG_VERSION").unwrap_or("0.1.0")
            )),
        }
    }
}

// --- OpenAI Specific Retry Logic (internal to the client) --- (remains the same)
#[derive(Clone, Debug, Default)]
struct ClientRetryLogic;

impl RetryLogic for ClientRetryLogic {
    type Error = GenericHttpError;
    type Response = ReqwestResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            GenericHttpError::Transport { source } => {
                warn!(target: "openai_client::retry", error_source=?source, "Retrying due to transport error");
                true
            }
            GenericHttpError::Timeout => {
                warn!(target: "openai_client::retry", "Retrying due to timeout error.");
                true
            }
            GenericHttpError::ServerError { status, body } => {
                let s = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if s.is_server_error()
                    || s == StatusCode::TOO_MANY_REQUESTS
                    || s == StatusCode::CONFLICT
                {
                    warn!(target: "openai_client::retry", %status, error_body=body.chars().take(100).collect::<String>(), "Retrying server error");
                    true
                } else {
                    debug!(target: "openai_client::retry", %status, error_body=body.chars().take(100).collect::<String>(), "Not retrying server error (non-retriable status)");
                    false
                }
            }
            _ => {
                debug!(target: "openai_client::retry", error_type=%error, "Not retrying client-side or unhandled error");
                false
            }
        }
    }

    fn should_retry_response(&self, response: &Self::Response) -> RetryAction {
        let status = response.status();
        if status.is_success() {
            RetryAction::Successful
        } else if status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::CONFLICT
            || status.is_server_error()
        {
            warn!(target: "openai_client::retry", %status, "Instructing to retry due to response status");
            RetryAction::Retry(Cow::Owned(format!(
                "Server responded with status {}",
                status
            )))
        } else {
            debug!(target: "openai_client::retry", %status, "Instructing not to retry (final client/unhandled error status)");
            RetryAction::DontRetry(Cow::Owned(format!(
                "Server responded with non-retriable status {}",
                status
            )))
        }
    }
}

// --- Request and Response Structs for Chat Completions --- (remains the same)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: Option<u32>,
    pub total_tokens: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub created: u64,
    pub model: String,
    pub system_fingerprint: Option<String>,
    pub object: String,
    pub usage: Option<Usage>,
}

// --- The OpenAI Client ---

// VVV CORRECTED TYPE ALIAS VVV
type InternalOpenAIService = AdaptiveConcurrencyLimit<
    Retry<ExponentialBackoffPolicy<ClientRetryLogic>, ReqwestService>, // Corrected: Use Retry<Policy, Service>
    ClientRetryLogic,
>;

#[derive(Clone)]
pub struct OpenAIClient {
    service: InternalOpenAIService,
    pub config: Arc<OpenAIClientConfig>,
    chat_completions_url: String,
}

impl OpenAIClient {
    pub fn new(config: OpenAIClientConfig) -> Result<Self, OpenAIClientError> {
        if config.api_key.is_empty() {
            return Err(OpenAIClientError::Initialization(
                "API key cannot be empty".to_string(),
            ));
        }

        let reqwest_client_to_use = config.reqwest_client.clone().unwrap_or_else(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(90)) // Increased default timeout
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| {
                    OpenAIClientError::Initialization(format!(
                        "Failed to build default reqwest client: {}",
                        e
                    ))
                })
                .expect("Building default reqwest client should not fail with basic config")
        });

        let reqwest_service = ReqwestService::new_with_client(reqwest_client_to_use);
        let client_retry_logic = ClientRetryLogic::default();

        let backoff_iterator_config = ExponentialBackoff::new(
            config.retry_initial_backoff_ms,
            config.retry_exp_base,
            Some(Duration::from_secs(config.retry_max_single_delay_secs)),
        )
        .factor(1);

        let retry_policy = ExponentialBackoffPolicy::new(
            config.retry_max_attempts,
            backoff_iterator_config,
            client_retry_logic.clone(), // For RetryLayer
            JitterMode::Full,
            Some(Duration::from_secs(5 * 60)), // Informational total max retry time
        );

        // Service wrapped by RetryLayer
        let retrying_reqwest_service = ServiceBuilder::new()
            .layer(RetryLayer::new(retry_policy))
            .service(reqwest_service);

        // AdaptiveConcurrencyLimitLayer wraps the retrying service
        let acl_service = ServiceBuilder::new()
            .layer(AdaptiveConcurrencyLimitLayer::new(
                None, // Adaptive mode
                config.adaptive_concurrency.clone(),
                client_retry_logic, // For ACL to interpret final errors after retries
            ))
            .service(retrying_reqwest_service); // Pass the service that already has retry logic

        let chat_completions_url = format!(
            "{}/v1/chat/completions",
            config.base_url.trim_end_matches('/')
        );

        info!(target: "openai_client", base_url = %config.base_url, default_model = %config.default_model, "OpenAIClient initialized");
        debug!(target: "openai_client", client_config = ?config, "Full client configuration");

        Ok(Self {
            service: acl_service,
            config: Arc::new(config),
            chat_completions_url,
        })
    }

    pub async fn chat_completion(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, OpenAIClientError> {
        debug!(target: "openai_client", model = %request.model, num_messages = request.messages.len(), "Preparing chat completion request");

        let payload_bytes =
            Bytes::from(serde_json::to_vec(&request).map_err(OpenAIClientError::Serialization)?);

        let mut http_req_builder = HttpRequest::builder()
            .method("POST")
            .uri(self.chat_completions_url.as_str())
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json");

        if let Some(ua) = &self.config.user_agent {
            http_req_builder = http_req_builder.header(
                reqwest::header::USER_AGENT,
                HeaderValue::from_str(ua).map_err(|e| {
                    OpenAIClientError::RequestBuild(format!("Invalid User-Agent: {}", e))
                })?,
            );
        }

        let http_req = http_req_builder
            .body(Some(payload_bytes))
            .map_err(|e| OpenAIClientError::RequestBuild(e.to_string()))?;

        self.service
            .ready()
            .await
            .map_err(|e| OpenAIClientError::ServiceError)?;
        // map_err(OpenAIClientError::ServiceError)?;
        debug!(target: "openai_client", "Service is ready, sending request to {}", self.chat_completions_url);

        let response = self
            .service
            .call(http_req)
            .await
            .map_err(|e| OpenAIClientError::ServiceError)?;
        let response_status = response.status(); // Store status before consuming body
        debug!(target: "openai_client", status = %response_status, "Received response from service");

        let response_body_text =
            response
                .text()
                .await
                .map_err(|e| OpenAIClientError::ApiError {
                    status: response_status.as_u16(),
                    error_body: format!("Failed to read response body: {}", e),
                })?;

        if response_status.is_success() {
            serde_json::from_str::<ChatCompletionResponse>(&response_body_text).map_err(|e| {
                OpenAIClientError::ResponseDeserialization {
                    status: response_status.as_u16(),
                    body_text: response_body_text,
                    source: e,
                }
            })
        } else {
            Err(OpenAIClientError::ApiError {
                status: response_status.as_u16(),
                error_body: response_body_text,
            })
        }
    }

    pub async fn chat_completion_with_messages(
        &mut self,
        messages: Vec<ChatMessage>,
    ) -> Result<ChatCompletionResponse, OpenAIClientError> {
        let request = ChatCompletionRequest {
            model: self.config.default_model.clone(),
            messages,
            temperature: None, // Or set defaults from config
            max_tokens: None,  // Or set defaults from config
        };
        self.chat_completion(request).await
    }
}

// --- Tests --- (remains the same)
#[cfg(test)]
mod tests {
    use super::*;
    use tokio;
    // use tracing_subscriber; // No need to init tracing here if main example/test runner does it

    // Helper to initialize tracing once for tests in this module, if run standalone
    // fn init_tracing_for_test() {
    //     let _ = tracing_subscriber::fmt().with_env_filter("info,openai_client=debug,rate_limiter_aimd=debug").try_init();
    // }

    #[tokio::test]
    #[ignore]
    async fn test_chat_completion_client() {
        // init_tracing_for_test(); // Call if running tests in a way that main doesn't init tracing
        println!("Starting test_chat_completion_client. Ensure OPENAI_API_KEY is set.");
        let api_key =
            std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set for this test");

        let mut config = OpenAIClientConfig::default();
        config.api_key = api_key;
        config.default_model = "gpt-3.5-turbo".to_string(); // Ensure a known model for testing

        let mut client = OpenAIClient::new(config).expect("Failed to create client");

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "You are a helpful assistant that provides concise answers.".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "What is the capital of France in one word?".to_string(),
            },
        ];

        match client.chat_completion_with_messages(messages).await {
            Ok(response) => {
                info!("Test: Successfully received chat completion.");
                assert!(!response.choices.is_empty());
                let choice = &response.choices[0];
                info!("Test: Assistant response: {}", choice.message.content);
                assert!(choice.message.content.to_lowercase().contains("paris"));
            }
            Err(e) => {
                error!("Test: Error during chat completion: {}", e);
                panic!("Chat completion failed: {}", e);
            }
        }
    }
}
