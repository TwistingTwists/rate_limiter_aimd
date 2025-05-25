// examples/openai_chat_adaptive_concurrency.rs

use rate_limiter_aimd::{
    adaptive_concurrency::{
        AdaptiveConcurrencySettings,
        http::HttpError as GenericHttpError,
        layer::AdaptiveConcurrencyLimitLayer,
        reqwest_integration::ReqwestService,
        retries::{RetryAction, RetryLogic},
    },
    Error as CrateError, // Assuming Error is pub from lib.rs
};
use http::{Request as HttpRequest, StatusCode};
use reqwest::Response as ReqwestResponse;
use std::{borrow::Cow, time::Duration, env};
use tokio::time::sleep;
use tower::{ServiceBuilder, Service, ServiceExt};
use tracing::{info, warn, error}; // Added warn and error
use serde_json::json; // For constructing JSON payload

// --- Configuration ---
const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
const OPENAI_BASE_URL_ENV_VAR: &str = "OPENAI_API_BASE_URL";
const OPENAI_MODEL_NAME_ENV_VAR: &str = "OPENAI_MODEL_NAME";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions"; // API path for chat

const NUM_PROMPTS_TO_SEND: usize = 100;
const REQUEST_INTERVAL_MS: u64 = 20; // How quickly to spawn requests. Adaptive limiter controls actual dispatch.
const DEFAULT_MODEL_NAME: &str = "Qwen/Qwen3-235B-A22B-FP8"; // Default model if not set by env

// --- OpenAI Specific Retry Logic (adapted from previous example) ---
#[derive(Clone, Debug, Default)]
struct OpenAIRetryLogic;

impl RetryLogic for OpenAIRetryLogic {
    type Error = GenericHttpError;
    type Response = ReqwestResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            GenericHttpError::Transport { source } => {
                warn!("OpenAIRetryLogic: Retrying due to transport error: {:?}", source);
                true
            },
            GenericHttpError::Timeout => {
                warn!("OpenAIRetryLogic: Retrying due to timeout error.");
                true
            },
            GenericHttpError::ServerError { status, body } => {
                let s = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS {
                    warn!("OpenAIRetryLogic: Retrying due to server error status {} (Body: {:.100}): {:?}", status, body, error);
                    true
                } else {
                    error!("OpenAIRetryLogic: Not retrying server error status {} (Body: {:.100}): {:?}", status, body, error);
                    false
                }
            }
            _ => {
                error!("OpenAIRetryLogic: Not retrying non-server/transport/timeout error: {:?}", error);
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
            || status.is_server_error() // Any 5xx
        {
            warn!("OpenAIRetryLogic: Retrying due to status: {}", status);
            RetryAction::Retry(Cow::Owned(format!(
                "Server responded with status {}",
                status
            )))
        } else if status.is_client_error() { // 4xx errors that are not TOO_MANY_REQUESTS
            error!("OpenAIRetryLogic: Not retrying due to client error status: {}", status);
            RetryAction::DontRetry(Cow::Owned(format!(
                "Server responded with client error status {}",
                status
            )))
        } else {
            warn!("OpenAIRetryLogic: Not retrying due to unhandled status: {}", status);
            RetryAction::DontRetry(Cow::Owned(format!(
                "Server responded with unhandled status {}",
                status
            )))
        }
    }
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    // Load .env file if present
    dotenvy::dotenv().ok();

    // --- Get API Key ---
    let api_key = env::var(OPENAI_API_KEY_ENV_VAR)
        .map_err(|_| format!("Missing OpenAI API key environment variable: {}", OPENAI_API_KEY_ENV_VAR))?;

    // --- Get Base URL for the API ---
    let base_url = env::var(OPENAI_BASE_URL_ENV_VAR)
        .unwrap_or_else(|_| DEFAULT_OPENAI_BASE_URL.to_string());
    let chat_completions_url = format!("{}{}", base_url, CHAT_COMPLETIONS_PATH);

    // --- Get Model Name ---
    let model_name = env::var(OPENAI_MODEL_NAME_ENV_VAR)
        .unwrap_or_else(|_| DEFAULT_MODEL_NAME.to_string());


    info!("Using API endpoint: {}", chat_completions_url);
    info!("Using Model: {}", model_name);
    info!("Using API key: {}...", &api_key[..std::cmp::min(8, api_key.len())]);


    // --- Setup Reqwest Client and Service ---
    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60)) // Increased timeout for potentially longer completions
        .build()?;
    let reqwest_service = ReqwestService::new_with_client(reqwest_client);

    // --- Setup Adaptive Concurrency ---
    let ac_settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(2)       // Start with 2 concurrent requests
        .max_concurrency_limit(15)    // Allow up to 15 concurrent requests
        .ewma_alpha(0.3)              // Standard EWMA alpha
        .decrease_ratio(0.85)         // Decrease concurrency by 15% on backpressure
        .rtt_deviation_scale(2.0)     // Standard RTT deviation scale
        .build();

    let openai_retry_logic = OpenAIRetryLogic::default();

    let concurrency_layer = AdaptiveConcurrencyLimitLayer::new(
        None, // None means adaptive mode
        ac_settings,
        openai_retry_logic,
    );

    // --- Build the Tower Service ---
    let mut service = ServiceBuilder::new()
        .layer(concurrency_layer)
        .service(reqwest_service);

    info!("Service initialized. Starting to send {} prompts...", NUM_PROMPTS_TO_SEND);

    let mut join_handles = Vec::new();

    let prompts: Vec<String> = (0..NUM_PROMPTS_TO_SEND)
        .map(|i| format!("This is test prompt number {}. Please provide a short, concise answer about a random topic. Keep it under 30 words.", i + 1))
        .collect();

    for (i, prompt_content) in prompts.into_iter().enumerate() {
        let mut cloned_service = service.clone();
        let key_clone = api_key.clone();
        let url_clone = chat_completions_url.clone();
        let model_clone = model_name.clone();
        let task_id = i;

        let handle = tokio::spawn(async move {
            info!("[Task {}] Preparing request to {} with prompt: \"{:.30}...\"", task_id, url_clone, prompt_content);

            let request_payload = json!({
                "model": model_clone,
                "messages": [
                    {"role": "user", "content": prompt_content}
                ],
                "max_tokens": 50, // Keep responses short for testing
                "temperature": 0.7
            });

            let body_bytes = serde_json::to_vec(&request_payload)
                .map_err(|e| CrateError::from(format!("[Task {}] Failed to serialize payload: {}", task_id, e)))?;
            let body = reqwest::Body::from(body_bytes);

            let http_request = HttpRequest::builder()
                .method("POST")
                .uri(url_clone.as_str())
                .header("Authorization", format!("Bearer {}", key_clone))
                .header("Content-Type", "application/json")
                .body(Some(body))
                .map_err(|e| CrateError::from(format!("[Task {}] Failed to build request: {}", task_id, e)))?;

            info!("[Task {}] Waiting for service readiness...", task_id);
            // The error type from poll_ready needs to be mapped if it's not already CrateError
            if let Err(e) = cloned_service.ready().await {
                 let err_msg = format!("[Task {}] Service not ready: {:?}", task_id, e);
                 error!("{}", err_msg);
                 return Err(CrateError::from(err_msg));
            }


            info!("[Task {}] Calling service...", task_id);
            match cloned_service.call(http_request).await {
                Ok(response) => {
                    let status = response.status();
                    let response_body_text = response.text().await.unwrap_or_else(|_| String::from("Error reading response body"));
                    if status.is_success() {
                        info!("[Task {}] SUCCESS: Status {}, Body: {:.100}...", task_id, status, response_body_text.trim().replace('\n', " "));
                    } else {
                        warn!("[Task {}] API_ERROR: Status {}, Body: {}", task_id, status, response_body_text);
                    }
                }
                Err(e) => {
                    // The error `e` here is already `CrateError` due to the service stack
                    error!("[Task {}] SERVICE_ERROR: {:?}", task_id, e);
                }
            }
            Ok::<(), CrateError>(())
        });
        join_handles.push(handle);
        if REQUEST_INTERVAL_MS > 0 {
            sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await;
        }
    }

    for (idx, handle) in join_handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(_)) => info!("[Main] Task {} completed successfully.", idx),
            Ok(Err(e)) => error!("[Main] Task {} completed with an application error: {:?}", idx, e),
            Err(e) => error!("[Main] Task {} panicked or was cancelled: {:?}", idx, e),
        }
    }

    info!("All {} prompts processed.", NUM_PROMPTS_TO_SEND);
    Ok(())
}