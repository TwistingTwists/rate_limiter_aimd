// examples/openai_chat_adaptive_concurrency.rs

use bytes::Bytes;
use http::{Request as HttpRequest, StatusCode};
use rate_limiter_aimd::{
    Error as CrateError,
    adaptive_concurrency::{
        AdaptiveConcurrencySettings,
        http::HttpError as GenericHttpError,
        layer::AdaptiveConcurrencyLimitLayer,
        reqwest_integration::ReqwestService,
        // VVV MODIFIED/ADDED IMPORTS VVV
        retries::{
            ExponentialBackoff,       // Added ExponentialBackoff
            ExponentialBackoffPolicy, // Added new policy
            JitterMode,
            RetryAction,
            RetryLogic,
        },
    },
};
use reqwest::Response as ReqwestResponse;
use serde_json::json;
use std::{borrow::Cow, env, time::Duration};
use tokio::time::sleep;
use tower::{Service, ServiceBuilder, ServiceExt, retry::RetryLayer};
use tracing::{Level, debug, error, info, warn};
use tracing_appender;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

// --- Configuration ---
const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
const OPENAI_BASE_URL_ENV_VAR: &str = "OPENAI_API_BASE_URL";
const OPENAI_MODEL_NAME_ENV_VAR: &str = "OPENAI_MODEL_NAME";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const LOG_FILE_NAME: &str = "openai_adaptive_concurrency.log";

const NUM_PROMPTS_TO_SEND: usize = 100;
const REQUEST_INTERVAL_MS: u64 = 20;
const DEFAULT_MODEL_NAME: &str = "Qwen/Qwen3-235B-A22B-FP8"; // Or your preferred default

// VVV NEW RETRY CONFIGURATION VVV
const MAX_INDIVIDUAL_REQUEST_ATTEMPTS: usize = 10; // Total attempts (1 initial + 9 retries)
const INITIAL_RETRY_BACKOFF_MS: u64 = 1000; // Start with 1 second
const EXPONENTIAL_BACKOFF_BASE: u64 = 2; // Double the delay each time
const MAX_SINGLE_RETRY_DELAY_SECS: u64 = 60; // Cap individual delays at 60 seconds
const MAX_TOTAL_RETRY_DURATION_MINS: u64 = 5; // Informational: Policy aims for this with attempts

// --- OpenAI Specific Retry Logic (remains the same) ---
#[derive(Clone, Debug, Default)]
struct OpenAIRetryLogic;
// ... (implementation of OpenAIRetryLogic remains identical to your previous version) ...
impl RetryLogic for OpenAIRetryLogic {
    type Error = GenericHttpError; // Error type from ReqwestService
    type Response = ReqwestResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            GenericHttpError::Transport { source } => {
                warn!(error_source=?source, "OpenAIRetryLogic: Retrying due to transport error");
                true
            }
            GenericHttpError::Timeout => {
                warn!("OpenAIRetryLogic: Retrying due to timeout error.");
                true
            }
            GenericHttpError::ServerError { status, body } => {
                let s = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS {
                    warn!(%status, error_body=body.chars().take(100).collect::<String>(), "OpenAIRetryLogic: Retrying due to server error");
                    true
                } else {
                    // Client errors (4xx other than 429) are usually not retriable with the same request.
                    error!(%status, error_body=body.chars().take(100).collect::<String>(), "OpenAIRetryLogic: Not retrying server/client error");
                    false
                }
            }
            // Explicitly non-retriable errors
            GenericHttpError::InvalidRequest { .. }
            | GenericHttpError::BuildRequest { .. }
            | GenericHttpError::ClientError { .. } => {
                error!(full_error=?error, "OpenAIRetryLogic: Not retrying client-side/invalid request error");
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
            || status.is_server_error()
        {
            warn!(%status, "OpenAIRetryLogic: Instructing to retry due to response status");
            RetryAction::Retry(Cow::Owned(format!(
                "Server responded with status {}",
                status
            )))
        } else if status.is_client_error() {
            error!(%status, "OpenAIRetryLogic: Instructing not to retry due to client error status");
            RetryAction::DontRetry(Cow::Owned(format!(
                "Server responded with client error status {}",
                status
            )))
        } else {
            warn!(%status, "OpenAIRetryLogic: Instructing not to retry due to unhandled status");
            RetryAction::DontRetry(Cow::Owned(format!(
                "Server responded with unhandled status {}",
                status
            )))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    // ... (tracing and env setup remains the same) ...
    let file_appender = tracing_appender::rolling::daily(".", LOG_FILE_NAME);
    let (non_blocking_appender, _guard) = tracing_appender::non_blocking(file_appender);

    let default_filter = "info,rate_limiter_aimd::adaptive_concurrency::stats=debug,rate_limiter_aimd::adaptive_concurrency::retries=debug"; // Added retries debug
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let console_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .with_target(true)
        .with_level(true);
    let file_layer = fmt::layer()
        .with_writer(non_blocking_appender)
        .with_ansi(false)
        .with_target(true)
        .with_level(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .try_init()?;

    info!(
        "Tracing initialized. Default filter: '{}'. Logging to console and {}",
        default_filter, LOG_FILE_NAME
    );
    dotenvy::dotenv().ok();
    info!("Attempted to load .env file");

    let api_key = env::var(OPENAI_API_KEY_ENV_VAR).map_err(|e| {
        format!(
            "Missing OpenAI API key env var: {}: {}",
            OPENAI_API_KEY_ENV_VAR, e
        )
    })?;
    let base_url =
        env::var(OPENAI_BASE_URL_ENV_VAR).unwrap_or_else(|_| DEFAULT_OPENAI_BASE_URL.to_string());
    let chat_completions_url = format!("{}{}", base_url, CHAT_COMPLETIONS_PATH);
    let model_name =
        env::var(OPENAI_MODEL_NAME_ENV_VAR).unwrap_or_else(|_| DEFAULT_MODEL_NAME.to_string());

    info!(target: "config", api_endpoint = %chat_completions_url, model = %model_name, api_key_prefix = %api_key[..std::cmp::min(8, api_key.len())]);

    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let reqwest_service = ReqwestService::new_with_client(reqwest_client);

    let ac_settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(2) // Start lower due to aggressive retries
        .max_concurrency_limit(8) // Lower max also due to aggressive retries
        .ewma_alpha(0.3)
        .decrease_ratio(0.70) // More aggressive decrease
        .rtt_deviation_scale(1.25) // More sensitive to RTT increases
        .build();
    info!(target: "config", adaptive_concurrency_settings = ?ac_settings);

    let openai_retry_logic = OpenAIRetryLogic::default();

    // VVV SETUP EXPONENTIAL BACKOFF POLICY VVV
    let backoff_iterator_config = ExponentialBackoff::new(
        INITIAL_RETRY_BACKOFF_MS, // Initial delay in ms
        EXPONENTIAL_BACKOFF_BASE, // Base for exponentiation (e.g., 2 for doubling)
        Some(Duration::from_secs(MAX_SINGLE_RETRY_DELAY_SECS)), // Max single delay
    )
    .factor(1); // Ensure factor is 1 as `current` in `new` is already in ms.

    let retry_policy = ExponentialBackoffPolicy::new(
        MAX_INDIVIDUAL_REQUEST_ATTEMPTS,
        backoff_iterator_config,
        openai_retry_logic.clone(), // Clone for RetryLayer
        JitterMode::Full,
        Some(Duration::from_secs(MAX_TOTAL_RETRY_DURATION_MINS * 60)), // Informational
    );
    info!(target: "config",
        retry_policy_max_attempts = MAX_INDIVIDUAL_REQUEST_ATTEMPTS,
        initial_backoff_ms = INITIAL_RETRY_BACKOFF_MS,
        exponential_base = EXPONENTIAL_BACKOFF_BASE,
        max_single_retry_delay_s = MAX_SINGLE_RETRY_DELAY_SECS,
        max_total_duration_mins = MAX_TOTAL_RETRY_DURATION_MINS
    );

    let concurrency_layer = AdaptiveConcurrencyLimitLayer::new(
        None,
        ac_settings,
        openai_retry_logic, // Clone for AC layer
    );

    let retrying_reqwest_service = ServiceBuilder::new()
        .layer(RetryLayer::new(retry_policy))
        .service(reqwest_service);

    let mut service = ServiceBuilder::new()
        .layer(concurrency_layer)
        .service(retrying_reqwest_service);

    // ... (rest of the main function: task spawning, request building with Bytes, calls, and result handling remains the same) ...
    info!(
        "Service initialized. Starting to send {} prompts...",
        NUM_PROMPTS_TO_SEND
    );
    let mut join_handles = Vec::new();
    let prompts: Vec<String> = (0..NUM_PROMPTS_TO_SEND)
        .map(|i| format!("This is test prompt number {}. Please provide a short, concise answer about a random topic. Keep it under 30 words.", i + 1))
        .collect();

    for (i, prompt_content) in prompts.into_iter().enumerate() {
        let mut cloned_service = service.clone(); // This service is AdaptiveConcurrencyLimit<Retry<...>>
        let key_clone = api_key.clone();
        let url_clone = chat_completions_url.clone();
        let model_clone = model_name.clone();
        let task_id = i;

        let handle = tokio::spawn(async move {
            let request_payload = json!({
                "model": model_clone, "messages": [{"role": "user", "content": prompt_content}],
                "max_tokens": 50, "temperature": 0.7
            });

            let body_bytes = Bytes::from(
                serde_json::to_vec(&request_payload)
                    .map_err(|e| CrateError::from(format!("[T{}] PayloadErr: {}", task_id, e)))?,
            );

            let http_request = HttpRequest::builder()
                .method("POST")
                .uri(url_clone.as_str())
                .header("Authorization", format!("Bearer {}", key_clone))
                .header("Content-Type", "application/json")
                .body(Some(body_bytes))
                .map_err(|e| CrateError::from(format!("[T{}] BuildReqErr: {}", task_id, e)))?;

            debug!("[T{}] Waiting for service readiness...", task_id);
            if let Err(e) = cloned_service.ready().await {
                let err_msg = format!("[T{}] NotReady: {:?}", task_id, e);
                error!("{}", err_msg);
                return Err(CrateError::from(err_msg));
            }

            info!("[T{}] Calling service...", task_id);
            match cloned_service.call(http_request).await {
                Ok(response) => {
                    let status = response.status();
                    let response_body_text = response
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("ErrReadBody: {}", e));
                    if status.is_success() {
                        info!(
                            "[T{}] SUCCESS: {} Body: {}",
                            task_id, status, response_body_text
                        );
                    } else {
                        warn!(
                            "[T{}] API_ERR_FINAL: {} Body: {}",
                            task_id, status, response_body_text
                        );
                    }
                }
                Err(e) => error!("[T{}] SVC_ERR_FINAL: {:?}", task_id, e),
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
            Ok(Ok(_)) => info!("[Main] Task {} completed.", idx),
            Ok(Err(e)) => error!("[Main] Task {} failed with app error: {:?}", idx, e),
            Err(e) => error!("[Main] Task {} panicked/cancelled: {:?}", idx, e),
        }
    }
    info!(
        "All {} prompts processed. Check {} for logs.",
        NUM_PROMPTS_TO_SEND, LOG_FILE_NAME
    );
    Ok(())
}
