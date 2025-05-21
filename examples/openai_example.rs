// examples/openai_example.rs

// This example demonstrates how to interact with an OpenAI-compatible API (e.g., OpenAI's Chat Completions API)
// using a robust HTTP client built with the Tower ecosystem.
//
// Key features demonstrated:
// - Building a Tower service stack with layers for:
//   - Timeouts: Limiting the duration of a request.
//   - Retries: Automatically retrying failed requests with configurable backoff (Fibonacci).
//   - Adaptive Concurrency Limiting: Adjusting the number of concurrent requests based on observed latency,
//     to prevent overloading the service and to maintain optimal performance. This uses the
//     `rate_limiter_aimd` crate's `AdaptiveConcurrencyLimitLayer`.
// - Making concurrent API calls using `futures::future::join_all`.
// - Deserializing API responses into strongly-typed Rust structs.
// - Using `tracing` for structured logging.
// - Configuring the API endpoint, key, and model via environment variables.
//
// Note: This example uses `reqwest::Body` and `http::Request` directly. In a real application,
// you might abstract this further into a dedicated client struct.

use serde::{Deserialize, Serialize};
use http::{Request, Method, Uri, HeaderValue, HeaderMap};
use reqwest::Body;
use serde_json; // Ensure serde_json is imported

use tower::{ServiceBuilder, Layer, Service, ServiceExt, BoxError};
use tower::retry::{RetryLayer, FibonacciBackoff};
use tower::timeout::TimeoutLayer;
use std::time::Duration;
use std::future::Future; // Required for `impl Future`
use rate_limiter_aimd::adaptive_concurrency::layer::AdaptiveConcurrencyLimitLayer;
use rate_limiter_aimd::adaptive_concurrency::retries::{DefaultReqwestRetryLogic, FibonacciRetryPolicy};
use rate_limiter_aimd::adaptive_concurrency::reqwest_integration::ReqwestService;
use rate_limiter_aimd::adaptive_concurrency::AdaptiveConcurrencySettings;
// Assuming crate::adaptive_concurrency::http::HttpError is the error type for ReqwestService
// use rate_limiter_aimd::adaptive_concurrency::http::HttpError as GenericHttpError;

use std::env;
use tokio; // For the async runtime
use tracing_subscriber::{EnvFilter, fmt as tracing_fmt}; // For logging
use futures::future::join_all; // For running multiple futures concurrently
use tracing::{info, error, warn}; // Import tracing macros

/// Errors that can occur during the OpenAI client operations, specifically during request creation.
#[derive(Debug)]
pub enum OpenAIClientError {
    /// Error during serialization of the request payload.
    Serialization(serde_json::Error),
    /// Error during the construction of the HTTP request (e.g., invalid headers).
    HttpConstruction(http::Error),
    /// Error due to an invalid URL string.
    InvalidUrl(String),
}

impl std::fmt::Display for OpenAIClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenAIClientError::Serialization(e) => write!(f, "Serialization error: {}", e),
            OpenAIClientError::HttpConstruction(e) => write!(f, "HTTP construction error: {}", e),
            OpenAIClientError::InvalidUrl(e) => write!(f, "Invalid URL: {}", e),
        }
    }
}

impl std::error::Error for OpenAIClientError {}

/// Creates an HTTP POST request for the OpenAI Chat Completions API.
///
/// This function constructs an `http::Request` with the necessary headers (Authorization, Content-Type)
/// and a JSON serialized body from the `request_payload`.
///
/// # Arguments
/// * `api_key` - The OpenAI API key for authentication.
/// * `base_url` - The base URL for the OpenAI API (e.g., "https://api.openai.com/v1").
/// * `request_payload` - A reference to the `ChatCompletionRequest` struct containing the model, messages, etc.
///
/// # Returns
/// A `Result` containing the constructed `http::Request<Option<reqwest::Body>>` on success,
/// or an `OpenAIClientError` on failure (e.g., serialization error, invalid URL, header construction error).
pub fn create_openai_http_request(
    api_key: &str,
    base_url: &str, // Example: "https://api.openai.com/v1"
    request_payload: &ChatCompletionRequest,
) -> Result<Request<Option<Body>>, OpenAIClientError> {
    let full_url = format!("{}/chat/completions", base_url); // Target specific endpoint

    // Serialize the request payload to JSON.

    let body_bytes = serde_json::to_vec(request_payload)
        .map_err(OpenAIClientError::Serialization)?;

    // Prepare headers.
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", api_key))
            .map_err(|e| OpenAIClientError::HttpConstruction(http::Error::from(e)))? // Convert InvalidHeaderValue to http::Error
    );
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"), // OpenAI API expects JSON.
    );

    // Parse the full URL into a URI.
    let uri = full_url.parse::<Uri>()
        .map_err(|e| OpenAIClientError::InvalidUrl(format!("Failed to parse URI '{}': {}", full_url, e)))?;

    // Build the request.
    // Note: `reqwest::Body` is used here, which can be created from `Vec<u8>`.
    // `Request::body` expects `Option<B>`, where `B` is `reqwest::Body` in this context.
    let builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Some(Body::from(body_bytes))) // Wrap Body in Option
        .map_err(OpenAIClientError::HttpConstruction)?;
    
    // Manually insert headers into the request's parts.
    // This is because `Request::builder().header()` would consume the builder,
    // and we are setting multiple headers. An alternative is to use `parts.headers.extend()`.
    let (mut parts, body) = builder.into_parts();
    parts.headers = headers;
    Ok(Request::from_parts(parts, body))
}

// Placeholder for GenericHttpError if not defined. Replace with actual import.
// This comment is a reminder that the actual error type from `ReqwestService` might need
// to be handled or aliased if it's not `BoxError` already or if specific error matching is needed.
// For this example, `BoxError::from` is used to simplify type signatures.
// use crate::adaptive_concurrency::http::HttpError as GenericHttpError;

/// Builds a Tower service stack for interacting with the OpenAI API.
///
/// The service stack includes the following layers, applied from outermost to innermost:
/// 1.  **TimeoutLayer**: Applies a timeout to each request.
/// 2.  **AdaptiveConcurrencyLimitLayer**: Limits the number of concurrent requests based on observed latency
///     and signals like HTTP 429 (Too Many Requests). It uses the `DefaultReqwestRetryLogic`
///     to interpret responses for rate limiting purposes.
/// 3.  **RetryLayer**: Retries failed requests (e.g., due to network issues or server errors like 5xx)
///     using a Fibonacci backoff strategy with jitter. It also uses `DefaultReqwestRetryLogic`.
/// 4.  **ReqwestService**: The underlying HTTP client service that sends requests using `reqwest`.
///
/// # Arguments
/// * `initial_concurrency_param` - The initial concurrency limit for the `AdaptiveConcurrencyLimitLayer`.
/// * `_min_concurrency` - (Currently unused by `AdaptiveConcurrencySettings`) A conceptual minimum concurrency.
/// * `max_concurrency_param` - The maximum concurrency limit for the `AdaptiveConcurrencyLimitLayer`.
/// * `max_retries` - The maximum number of retries for the `RetryLayer`.
/// * `initial_backoff_ms` - The initial backoff duration in milliseconds for retries.
/// * `max_backoff_ms` - The maximum backoff duration in milliseconds for retries.
/// * `request_timeout_ms` - The timeout for each individual request in milliseconds.
///
/// # Returns
/// An implementation of `tower::Service` that is `Clone`, takes `http::Request<Option<reqwest::Body>>`,
/// and returns a `Future` resolving to `Result<reqwest::Response, BoxError>`.
/// The `BoxError` is used to type-erase the error types from different layers.
pub fn build_openai_service(
    // Concurrency settings
    initial_concurrency_param: usize,
    _min_concurrency: usize, // Note: AdaptiveConcurrencySettings does not have a direct 'min_concurrency' field.
                             // `initial_concurrency` serves as the starting point.
    max_concurrency_param: usize,
    // Retry settings
    max_retries: usize,
    initial_backoff_ms: u64,
    max_backoff_ms: u64,
    // Timeout settings
    request_timeout_ms: u64,
) -> impl Service<
    http::Request<Option<reqwest::Body>>, 
    Response = reqwest::Response,          
    Error = BoxError,                      
    Future = impl Future<Output = Result<reqwest::Response, BoxError>> + Send + 'static 
> + Clone {
    // 1. Retry Logic & Policy: Defines how and when to retry requests.
    let retry_logic = DefaultReqwestRetryLogic; // Uses HTTP status codes (e.g., 429, 5xx) to decide if a retry is warranted.
    let retry_policy = FibonacciRetryPolicy::new(
        max_retries,
        Duration::from_millis(initial_backoff_ms),
        Duration::from_millis(max_backoff_ms),
        retry_logic.clone(), // The logic to classify responses for retrying.
        rate_limiter_aimd::adaptive_concurrency::retries::JitterMode::Full, // Adds jitter to backoff to prevent thundering herd.
    );
    let retry_layer = RetryLayer::new(retry_policy);

    // 2. Adaptive Concurrency Settings & Layer: Controls concurrent requests dynamically.
    let ac_settings = AdaptiveConcurrencySettings {
        initial_concurrency: initial_concurrency_param,
        max_concurrency_limit: max_concurrency_param,
        // `_min_concurrency` is not a direct field in `AdaptiveConcurrencySettings`.
        // Other fields (decrease_ratio, ewma_alpha, rtt_deviation_scale) use their default values.
        ..Default::default() 
    };
    // The `AdaptiveConcurrencyLimitLayer` also uses `retry_logic` to understand signals like HTTP 429.
    let adaptive_concurrency_layer = AdaptiveConcurrencyLimitLayer::new(
        Some(initial_concurrency_param), // Sets the initial limit for the semaphore in the layer.
        ac_settings,
        retry_logic, 
    );

    // 3. Timeout Layer: Enforces a timeout for each request.
    let timeout_layer = TimeoutLayer::new(Duration::from_millis(request_timeout_ms));

    // 4. Reqwest Service: The actual HTTP client performing the requests.
    let reqwest_service = ReqwestService::new(); // Uses a default reqwest client.
                                                 // Use `ReqwestService::new_with_client(custom_client)` for a custom reqwest::Client.

    // 5. Compose the service stack using ServiceBuilder.
    // The order of layers is crucial:
    // - Outermost: TimeoutLayer (applied first to the entire request lifecycle including retries)
    // - Middle: AdaptiveConcurrencyLimitLayer (manages concurrency before a request is attempted/retried)
    // - Inner: RetryLayer (handles retries for the underlying service)
    // - Innermost: ReqwestService (the client making the actual HTTP call)
    ServiceBuilder::new()
        .layer(timeout_layer) // Applies timeout to each attempt, including those managed by retry/concurrency layers.
        .layer(adaptive_concurrency_layer) // Limits concurrency for requests passing through.
        .layer(retry_layer) // Retries requests to the `reqwest_service`.
        .service(reqwest_service) // The core service that sends HTTP requests.
        .map_err(BoxError::from) // Converts errors from any layer into `BoxError` for a unified error type.
}

// --- Data Structures for OpenAI API ---

/// Represents a request payload for the OpenAI Chat Completions API.
#[derive(Debug, Serialize, Clone)]
pub struct ChatCompletionRequest {
    /// The model to use for the chat completion (e.g., "gpt-3.5-turbo").
    pub model: String,
    /// A list of messages forming the conversation history.
    pub messages: Vec<ChatMessage>,
    // Common optional parameters like temperature, max_tokens can be added here.
    // Example:
    // /// Optional: What sampling temperature to use, between 0 and 2.
    // pub temperature: Option<f32>,
    // /// Optional: The maximum number of tokens to generate in the chat completion.
    // pub max_tokens: Option<u32>,
}

/// Represents a single message in a chat conversation, used in both requests and responses.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    /// The role of the message sender (e.g., "system", "user", "assistant").
    pub role: String,
    /// The content of the message.
    pub content: String,
}

/// Represents the overall response structure from the OpenAI Chat Completions API.
#[derive(Debug, Deserialize)]
pub struct ChatCompletionResponse {
    /// A unique identifier for the chat completion.
    pub id: String,
    /// The object type, typically "chat.completion".
    pub object: String,
    /// Unix timestamp (seconds) of when the chat completion was created.
    pub created: u64,
    /// The model used for the chat completion.
    pub model: String,
    /// A list of chat completion choices. Usually, `n` is 1, so there's one choice.
    pub choices: Vec<Choice>,
    /// Optional: Usage statistics for the chat completion request.
    pub usage: Option<Usage>,
}

/// Represents a single choice (i.e., a potential completion) in the chat response.
#[derive(Debug, Deserialize)]
pub struct Choice {
    /// The index of the choice in the list of choices.
    pub index: u32,
    /// The message generated by the model.
    pub message: ChatMessage,
    /// The reason the model stopped generating tokens (e.g., "stop", "length").
    pub finish_reason: String,
}

/// Represents token usage statistics for a chat completion request.
#[derive(Debug, Deserialize)]
pub struct Usage {
    /// The number of tokens in the prompt.
    pub prompt_tokens: u32,
    /// Optional: The number of tokens in the generated completion.
    /// This might not be present in all streaming scenarios until the end.
    pub completion_tokens: Option<u32>,
    /// The total number of tokens used in the request (prompt + completion).
    pub total_tokens: u32,
}

/// Main entry point for the OpenAI Tower example application.
///
/// This asynchronous function demonstrates the end-to-end process of:
/// 1. Initializing `tracing` for logging.
/// 2. Reading configuration (API key, base URL, model name) from environment variables:
///    - `OPENAI_API_KEY`: Your OpenAI API key (required).
///    - `OPENAI_BASE_URL`: The base URL for the API (defaults to "https://api.openai.com/v1").
///    - `OPENAI_MODEL`: The model to use (defaults to "gpt-3.5-turbo").
/// 3. Building the robust Tower service stack using `build_openai_service`.
/// 4. Creating a series of example chat completion requests.
/// 5. Making concurrent API calls using the service and `futures::future::join_all`.
/// 6. Handling responses:
///    - Checking HTTP status codes.
///    - Deserializing successful JSON responses into `ChatCompletionResponse`.
///    - Logging key information, including the generated message or errors.
/// 7. Logging the outcome of each request.
#[tokio::main]
async fn main() {
    // Initialize tracing subscriber for logging.
    // Uses RUST_LOG environment variable for log level configuration (e.g., RUST_LOG=info,openai_example=debug)
    // Defaults to "info" if RUST_LOG is not set.
    tracing_fmt::Subscriber::builder()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    info!("Starting OpenAI Tower Example application");

    // --- Configuration ---
    // Load API key from environment variable. This is essential.
    let api_key = env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY environment variable must be set for this example to work.");
    
    // Load base URL or use default OpenAI URL.
    let base_url = env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    
    // Load model name or use default.
    let model_name = env::var("OPENAI_MODEL")
        .unwrap_or_else(|_| "gpt-3.5-turbo".to_string());

    info!(base_url = %base_url, model = %model_name, "Using OpenAI API configuration");

    // --- Service Construction ---
    // Build the Tower service with specified parameters for concurrency, retries, and timeout.
    // These parameters can be tuned based on expected load and API behavior.
    let mut service = build_openai_service(
        5,    // initial_concurrency: Start with allowing 5 concurrent requests.
        2,    // min_concurrency (conceptual): Though not a direct setting in AdaptiveConcurrencySettings,
              // this represents a desired floor if the system were to scale down.
        20,   // max_concurrency: Allow up to 20 concurrent requests.
        3,    // max_retries: Retry failed requests up to 3 times.
        200,  // initial_backoff_ms: Start with 200ms backoff for retries.
        10000, // max_backoff_ms: Cap retry backoff at 10 seconds.
        30000, // request_timeout_ms: Timeout each request attempt after 30 seconds.
    );

    // --- Request Preparation ---
    // Create a vector of example chat completion requests.
    let requests_data = vec![
        ChatCompletionRequest {
            model: model_name.clone(),
            messages: vec![ChatMessage { role: "user".to_string(), content: "Tell me a joke about Rust programming.".to_string() }],
        },
        ChatCompletionRequest {
            model: model_name.clone(),
            messages: vec![ChatMessage { role: "user".to_string(), content: "What is the capital of France?".to_string() }],
        },
        ChatCompletionRequest {
            model: model_name.clone(),
            messages: vec![ChatMessage { role: "user".to_string(), content: "Explain the concept of Tower middleware in one sentence.".to_string() }],
        },
        // Add a few more requests to better demonstrate concurrency and rate limiting.
        ChatCompletionRequest {
            model: model_name.clone(),
            messages: vec![ChatMessage { role: "user".to_string(), content: "What is async/await in Rust?".to_string() }],
        },
        ChatCompletionRequest {
            model: model_name.clone(),
            messages: vec![ChatMessage { role: "user".to_string(), content: "Suggest a good name for a new programming language.".to_string() }],
        },
    ];

    let mut request_futures = Vec::new(); // Store futures for each request.

    // --- Concurrent Request Execution ---
    for (i, req_payload) in requests_data.into_iter().enumerate() {
        // Before making a call, it's crucial to ensure the service is ready.
        // `service.ready().await` returns a `Poll::Ready` or `Poll::Pending`,
        // and `await` resolves it to `Ok(service_instance)` or `Err`.
        match service.ready().await {
            Ok(ready_service) => {
                // Create the actual HTTP request object.
                let http_req = match create_openai_http_request(&api_key, &base_url, &req_payload) {
                    Ok(req) => req,
                    Err(e) => {
                        // Log error and skip this request if creation fails.
                        error!(request_index = i, error = %e, "Failed to create HTTP request for payload");
                        // Push a future that immediately resolves to an error for this request.
                        request_futures.push(async move { Err(format!("Request {}: Failed to create - {}", i, e)) });
                        continue; 
                    }
                };

                info!(request_index = i, "Sending request to API");
                
                // `ready_service.call(http_req)` returns a future.
                // We map this future to process the response or error.
                let fut = async move {
                    match ready_service.call(http_req).await {
                        Ok(response) => { // HTTP call was successful (may still be an API error status code)
                            let status = response.status();
                            info!(request_index = i, status = %status, "Received response from API");

                            if status.is_success() {
                                // Attempt to deserialize the successful response body as JSON.
                                match response.json::<ChatCompletionResponse>().await {
                                    Ok(chat_response) => {
                                        if let Some(choice) = chat_response.choices.first() {
                                            info!(request_index = i, response_id = %chat_response.id, message = %choice.message.content, "API call successful");
                                            // Return a summary of the successful response.
                                            Ok(format!("Request {}: Success - {}", i, choice.message.content.chars().take(70).collect::<String>().replace('\n', " ")))
                                        } else {
                                            warn!(request_index = i, response_id = %chat_response.id, "API response successful but no choices found.");
                                            Ok(format!("Request {}: Success (no choices)", i))
                                        }
                                    }
                                    Err(e) => {
                                        // This occurs if the response status is success, but body isn't valid JSON or doesn't match ChatCompletionResponse.
                                        error!(request_index = i, status = %status, error = %e, "Failed to parse JSON from successful API response");
                                        Err(format!("Request {}: Failed to parse JSON (status {}) - {}", i, status, e))
                                    }
                                }
                            } else { // API returned a non-success status code (e.g., 4xx, 5xx).
                                let error_body = response.text().await.unwrap_or_else(|e| format!("Could not read error body: {}", e));
                                error!(request_index = i, status = %status, error_body = %error_body, "API call failed with error status");
                                Err(format!("Request {}: Failed with status {} - {}", i, status, error_body.chars().take(100).collect::<String>().replace('\n', " ")))
                            }
                        }
                        Err(e) => { // The Tower service stack returned an error (e.g., timeout, too many retries, internal error).
                            error!(request_index = i, error = %e, "Service call failed");
                            Err(format!("Request {}: Service error - {}", i, e))
                        }
                    }
                };
                request_futures.push(fut);
            }
            Err(e) => { // The service is not ready (e.g., concurrency limit reached and it's not becoming free).
                error!(request_index = i, error = %e, "Service not ready, skipping request");
                // Push a future that immediately resolves to an error for this request.
                request_futures.push(async move { Err(format!("Request {}: Service not ready - {}", i, e)) });
            }
        }
    }

    info!("Processing all {} initiated requests...", request_futures.len());
    // `join_all` waits for all futures in the vector to complete.
    let results = join_all(request_futures).await;

    // --- Results ---
    // Log the outcome of each processed request.
    for (i, result) in results.iter().enumerate() {
        match result {
            Ok(summary) => info!(request_index = i, "Final Outcome: {}", summary),
            Err(summary) => error!(request_index = i, "Final Outcome: {}", summary),
        }
    }

    info!("OpenAI Tower Example application finished.");
}
