use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::Request as HttpRequest;
use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use tower::retry::Retry; // Corrected import: Removed non-existent 'Error as RetryError'
use tower::{Service, ServiceBuilder};
use url::Url;

use rate_limiter_aimd::adaptive_concurrency::http::HttpError as GenericHttpError;
use rate_limiter_aimd::Error as RateLimiterError; // Import the error type
use rate_limiter_aimd::adaptive_concurrency::layer::AdaptiveConcurrencyLimitLayer;
use rate_limiter_aimd::adaptive_concurrency::reqwest_integration::ReqwestService;
use rate_limiter_aimd::adaptive_concurrency::retries::{
    DefaultReqwestRetryLogic, ExponentialBackoff, ExponentialBackoffPolicy, JitterMode,
    RetryAction, RetryLogic,
};
use rate_limiter_aimd::adaptive_concurrency::service::AdaptiveConcurrencyLimit;
use rate_limiter_aimd::adaptive_concurrency::AdaptiveConcurrencySettings;

// --- Constants ---
const DEFAULT_GEMINI_API_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const DEFAULT_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

// --- Error Definition ---
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum GeminiClientError {
    #[snafu(display("Initialization error: {message}"))]
    Initialization { message: String },

    #[snafu(display("HTTP request construction error: {source}"))]
    RequestConstruction { source: http::Error },

    #[snafu(display("URL parsing error: {source}"))]
    UrlParse { source: url::ParseError },

    #[snafu(display("Reqwest client error: {source}"))]
    Reqwest { source: reqwest::Error },

    #[snafu(display("Error from adaptive concurrency limiter: {source}"))]
    AdaptiveConcurrency {
        source: RateLimiterError,
    },

    #[snafu(display("HTTP error: {source}"))]
    Http { source: GenericHttpError },

    #[snafu(display("Failed to deserialize JSON response: {source}"))]
    JsonDeserialization { source: serde_json::Error },

    #[snafu(display("Failed to serialize JSON request: {source}"))]
    JsonSerialization { source: serde_json::Error },

    #[snafu(display("Base URL does not support path segments: {url}"))]
    BaseUrlCannotHavePathSegments { url: String },

    #[snafu(display("API error: {code} - {message}"))]
    ApiError { code: u16, message: String },

    #[snafu(display("No content returned from API"))]
    NoContent,
}

// --- Configuration ---
#[derive(Clone, Debug)]
pub struct GeminiClientConfig {
    pub api_key: String,
    pub base_url: String,
    pub default_model: String,
    pub user_agent: Option<String>,
    pub reqwest_client: Option<ReqwestClient>,
    pub adaptive_concurrency: AdaptiveConcurrencySettings,
    pub retry_max_attempts: usize,
    pub retry_initial_backoff_ms: u64,
    pub retry_exp_base: u64, // For exponential backoff
    pub retry_max_single_delay_secs: u64,
}

impl Default for GeminiClientConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: DEFAULT_GEMINI_API_BASE_URL.to_string(),
            default_model: "gemini-pro".to_string(), // A common default
            user_agent: Some(DEFAULT_USER_AGENT.to_string()),
            reqwest_client: None,
            adaptive_concurrency: AdaptiveConcurrencySettings::default(),
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 200,
            retry_exp_base: 2,
            retry_max_single_delay_secs: 30,
        }
    }
}

// --- API Request/Response Structures (Simplified for generateContent) ---
// Refer to: https://ai.google.dev/api/rest/v1beta/models/generateContent
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub contents: Vec<Content>,
    // Add other fields like generation_config, safety_settings, tools, etc. as needed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_settings: Option<Vec<SafetySetting>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<Part>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>, // "user" or "model"
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    // Add other part types like inline_data, function_call, etc.
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SafetySetting {
    pub category: HarmCategory,
    pub threshold: HarmBlockThreshold,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HarmCategory {
    HarmCategoryHarassment,
    HarmCategoryHateSpeech,
    HarmCategorySexuallyExplicit,
    HarmCategoryDangerousContent,
    HarmCategoryUnspecified,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HarmBlockThreshold {
    HarmBlockThresholdUnspecified,
    BlockLowAndAbove,
    BlockMediumAndAbove,
    BlockOnlyHigh,
    BlockNone,
}


#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    pub candidates: Option<Vec<Candidate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_feedback: Option<PromptFeedback>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: Option<Content>, // Usually contains the model's response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>, // e.g., "STOP", "MAX_TOKENS", "SAFETY"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_ratings: Option<Vec<SafetyRating>>,
    // Add other fields like citation_metadata, token_count
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SafetyRating {
    pub category: HarmCategory,
    pub probability: String, // e.g., "NEGLIGIBLE", "LOW", "MEDIUM", "HIGH"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PromptFeedback {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<String>,
    pub safety_ratings: Vec<SafetyRating>,
}


// --- Gemini Client ---
/// Represents the underlying HTTP client service stack, including adaptive concurrency,
/// retries, and the base Reqwest service.
type HttpClientService = AdaptiveConcurrencyLimit<
    Retry<
        ExponentialBackoffPolicy<DefaultReqwestRetryLogic>,
        ReqwestService,
    >,
    DefaultReqwestRetryLogic, // This is the L for AdaptiveConcurrencyLimit
>;

#[derive(Clone)]
/// Client for interacting with the Google Gemini API.
///
/// It handles request construction, API communication, and response parsing.
/// It integrates adaptive concurrency control and retry mechanisms.
pub struct GeminiClient {
    service: HttpClientService,
    /// Shared configuration for the client.
    pub config: Arc<GeminiClientConfig>,
    // parsed_base_url: Url, // Parsed base URL for API requests.
    // api_key: String,      // API key, stored for easy access in URL construction.
}

impl Debug for GeminiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiClient")
            .field("config", &self.config)
            // service is not Debug, and other fields are derived from config or internal
            .finish_non_exhaustive() 
    }
}

impl GeminiClient {
    /// Creates a new `GeminiClient` instance.
    ///
    /// # Arguments
    ///
    /// * `config`: Configuration for the client.
    ///
    /// # Errors
    ///
    /// Returns `GeminiClientError::Initialization` if the API key is empty.
    /// Other errors can occur during the setup of underlying services.
    pub fn new(config: GeminiClientConfig) -> Result<Self, GeminiClientError> {
        if config.api_key.is_empty() {
            return Err(GeminiClientError::Initialization {
                message: "API key cannot be empty".to_string(),
            });
        }

        let reqwest_client_to_use = config.reqwest_client.clone().unwrap_or_else(|| {
            ReqwestClient::builder()
                .timeout(Duration::from_secs(90)) // Default timeout
                .connect_timeout(Duration::from_secs(10))
                .user_agent(config.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT))
                .build()
                .unwrap() // Builder::build() can fail if an invalid TLS backend is forced
        });

        let base_service = ReqwestService::new_with_client(reqwest_client_to_use);

        // Configure retry policy
        let retry_policy = ExponentialBackoffPolicy::new(
            config.retry_max_attempts,
            ExponentialBackoff::new(
                config.retry_initial_backoff_ms,
                config.retry_exp_base,
                Some(Duration::from_secs(config.retry_max_single_delay_secs)),
            ),
            DefaultReqwestRetryLogic::default(), // Replace with Gemini specific if needed
            JitterMode::Full, // Recommended for better distribution
            None, // No max total retry duration by default
        );

        let service = ServiceBuilder::new()
            .layer(AdaptiveConcurrencyLimitLayer::new( // ACL is now outermost over Retry
                None, // Use default initial concurrency from settings
                config.adaptive_concurrency.clone(),
                DefaultReqwestRetryLogic::default(), // Logic for ACL itself
            ))
            .retry(retry_policy) // Retry layer wraps the base_service directly
            .service(base_service);

        Ok(Self {
            service,
            config: Arc::new(config),
        })
    }

    /// Constructs the full URL for a given model and the `generateContent` endpoint.
    fn build_url_for_model(&self, model: &str) -> Result<Url, GeminiClientError> {
        // Start with the base URL from config (e.g., "https://.../v1beta/models")
        let mut endpoint_url = Url::parse(&self.config.base_url)
            .map_err(|e| GeminiClientError::UrlParse { source: e })?;

        // The segment to append, combining model and action, e.g., "gemini-pro:generateContent".
        // This segment will be added to the path of the base_url.
        let model_action_path_segment = format!("{}:generateContent", model);
        tracing::debug!(target: "gemini_client", base_url = %endpoint_url, model_action_segment = %model_action_path_segment, "Preparing to append model action segment");

        // Append the model_action_path_segment to the existing path.
        // If endpoint_url is "https://.../models", its path is "/models".
        // After push, path becomes "/models/gemini-pro:generateContent".
        // path_segments_mut() handles the logic of adding slashes correctly.
        endpoint_url.path_segments_mut()
            .map_err(|_| GeminiClientError::BaseUrlCannotHavePathSegments {
                url: self.config.base_url.clone(),
            })?
            .push(&model_action_path_segment);
        
        tracing::debug!(target: "gemini_client", url_after_path_append = %endpoint_url, "URL after appending path segment");

        // Add the API key as a query parameter.
        endpoint_url.query_pairs_mut().append_pair("key", &self.config.api_key);
        tracing::info!(target: "gemini_client", final_url = %endpoint_url, "Final endpoint URL with API key");

        Ok(endpoint_url)
    }

    /// Generates content based on the provided model and request.
    ///
    /// # Arguments
    ///
    /// * `model`: The model to use for content generation (e.g., "gemini-pro").
    /// * `request`: The `GenerateContentRequest` containing the prompts and configuration.
    ///
    /// # Errors
    ///
    /// Returns `GeminiClientError` if the request fails at any stage (construction,
    /// network, API error, deserialization).
    pub async fn generate_content(
        &mut self,
        model: &str, // e.g., "gemini-pro", "gemini-1.5-flash-latest"
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse, GeminiClientError> {
        let url = self.build_url_for_model(model)?;
        let body_bytes = serde_json::to_vec(&request)
            .map_err(|e| GeminiClientError::JsonSerialization { source: e })?;

        let http_request_builder = HttpRequest::builder()
            .method("POST")
            .uri(url.as_str())
            .header("Content-Type", "application/json");

        let http_request = http_request_builder.body(Some(Bytes::from(body_bytes)))
            .map_err(|e| {
                tracing::error!(target: "gemini_client", "HTTP request construction failed. Error (debug): {:?}, URL attempted: '{}'", e, url.as_str());
                GeminiClientError::RequestConstruction { source: e }
            })?;

        // Wait for the service to be ready
        futures::future::poll_fn(|cx| self.service.poll_ready(cx))
            .await
            .map_err(|e| GeminiClientError::AdaptiveConcurrency { source: e })?;

        // Call the service
        let response = self
            .service
            .call(http_request)
            .await
            .map_err(|e| GeminiClientError::AdaptiveConcurrency { source: e })?;

        // Process the response
        let status = response.status();
        let response_body = response
            .bytes()
            .await
            .map_err(|e| GeminiClientError::Reqwest { source: e })?;

        if status.is_success() {
            serde_json::from_slice(&response_body)
                .map_err(|e| GeminiClientError::JsonDeserialization { source: e })
        } else {
            // Try to parse as Google API Error if possible, otherwise generic
            // Google API errors often look like: {"error": {"code": 400, "message": "API key not valid...", "status": "INVALID_ARGUMENT"}}
            #[derive(Deserialize)]
            struct GoogleApiErrorWrapper { error: GoogleApiError }
            #[derive(Deserialize)]
            struct GoogleApiError { code: u16, message: String, status: Option<String> }

            if let Ok(google_error_wrapper) = serde_json::from_slice::<GoogleApiErrorWrapper>(&response_body) {
                 Err(GeminiClientError::ApiError {
                    code: google_error_wrapper.error.code,
                    message: google_error_wrapper.error.message,
                })
            } else {
                let error_message = String::from_utf8_lossy(&response_body).into_owned();
                Err(GeminiClientError::ApiError {
                    code: status.as_u16(),
                    message: format!("Gemini API request failed with status {}: {}", status, error_message),
                })
            }
        }
    }

    /// Helper to make a content generation request using the client's default model.
    pub async fn generate_content_with_defaults(
        &mut self,
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse, GeminiClientError> {
        let default_model = self.config.default_model.clone();
        self.generate_content(&default_model, request).await
    }

    /// Simplified helper for generating text from a single user prompt string.
    ///
    /// This method constructs a basic `GenerateContentRequest` for a user role
    /// and extracts the text from the first candidate's first part in the response.
    ///
    /// # Arguments
    ///
    /// * `model`: The model to use.
    /// * `prompt`: The user's text prompt.
    ///
    /// # Errors
    ///
    /// Returns `GeminiClientError` on failure, including `NoContent` if the
    /// response structure doesn't yield a text part as expected.
    pub async fn generate_text_from_user_prompt(
        &mut self,
        model: &str,
        prompt: String,
    ) -> Result<String, GeminiClientError> {
        let request = GenerateContentRequest {
            contents: vec![Content {
                parts: Some(vec![Part { text: Some(prompt) }]),
                role: Some("user".to_string()),
            }],
            generation_config: None, // Add default config if desired
            safety_settings: None,
        };
        let response = self.generate_content(model, request).await?;
        
        response.candidates
            .and_then(|mut c| c.pop())
            .and_then(|cand| cand.content)
            .and_then(|con| con.parts)
            .and_then(|mut p| p.pop())
            .and_then(|part| part.text)
            .ok_or(GeminiClientError::NoContent)
    }
}

// --- Custom Retry Logic (Optional - Placeholder) ---
// You might want specific retry logic for Gemini, e.g., different status codes.
#[derive(Clone, Debug, Default)]
pub struct GeminiRetryLogic;

impl RetryLogic for GeminiRetryLogic {
    type Error = GenericHttpError;
    type Response = reqwest::Response;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        // Same as DefaultReqwestRetryLogic or customize
        matches!(error, GenericHttpError::Transport { .. } | GenericHttpError::Timeout)
    }

    fn should_retry_response(&self, response: &Self::Response) -> RetryAction {
        let status = response.status();
        if status.is_success() {
            RetryAction::Successful
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS ||
                  status == reqwest::StatusCode::SERVICE_UNAVAILABLE ||
                  status.is_server_error() {
            RetryAction::Retry(std::borrow::Cow::Owned(format!(
                "Server responded with status {}",
                status
            )))
        } else {
            // For Gemini, specific non-retriable errors might be 400 (Bad Request for malformed content, invalid API key)
            // or 403 (Permission Denied).
            // You might want to log these or handle them specifically.
            // For now, treat other client errors as non-retriable.
            RetryAction::DontRetry(std::borrow::Cow::Owned(format!(
                "Server responded with non-retriable status {}",
                status
            )))
        }
    }
}
