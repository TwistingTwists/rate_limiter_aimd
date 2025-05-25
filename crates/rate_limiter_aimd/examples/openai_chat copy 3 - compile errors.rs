// examples/openai_chat_adaptive_concurrency.rs

use rate_limiter_aimd::{
    adaptive_concurrency::{
        AdaptiveConcurrencySettings,
        http::HttpError as GenericHttpError,
        layer::AdaptiveConcurrencyLimitLayer,
        reqwest_integration::ReqwestService,
        // ADD THESE:
        retries::{RetryAction, RetryLogic, FibonacciRetryPolicy, JitterMode},
    },
    Error as CrateError,
};
use http::{Request as HttpRequest, StatusCode};
use reqwest::Response as ReqwestResponse;
use std::{borrow::Cow, time::Duration, env};
use tokio::time::sleep;
// ADD THIS:
use tower::{ServiceBuilder, Service, ServiceExt, retry::RetryLayer};
use tracing::{info, warn, error, Level};
use serde_json::json;
use tracing_subscriber::{fmt, EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use tracing_appender;

// --- Configuration --- (remains mostly the same)
const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
const OPENAI_BASE_URL_ENV_VAR: &str = "OPENAI_API_BASE_URL";
const OPENAI_MODEL_NAME_ENV_VAR: &str = "OPENAI_MODEL_NAME";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const LOG_FILE_NAME: &str = "openai_adaptive_concurrency.log";

const NUM_PROMPTS_TO_SEND: usize = 100;
const REQUEST_INTERVAL_MS: u64 = 20;
const DEFAULT_MODEL_NAME: &str = "Qwen/Qwen3-235B-A22B-FP8"; // Or your preferred default

// Max retries for an individual request by the RetryLayer
const MAX_INDIVIDUAL_REQUEST_RETRIES: usize = 5;
const INITIAL_RETRY_BACKOFF_SECS: u64 = 1;
const MAX_RETRY_BACKOFF_SECS: u64 = 30;


// --- OpenAI Specific Retry Logic (remains the same) ---
#[derive(Clone, Debug, Default)]
struct OpenAIRetryLogic;

impl RetryLogic for OpenAIRetryLogic {
    type Error = GenericHttpError;
    type Response = ReqwestResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            GenericHttpError::Transport { source } => {
                warn!(error_source=?source, "OpenAIRetryLogic: Retrying due to transport error");
                true
            },
            GenericHttpError::Timeout => {
                warn!("OpenAIRetryLogic: Retrying due to timeout error.");
                true
            },
            GenericHttpError::ServerError { status, body } => {
                let s = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS {
                    warn!(%status, error_body=body.chars().take(100).collect::<String>(), "OpenAIRetryLogic: Retrying due to server error");
                    true
                } else {
                    error!(%status, error_body=body.chars().take(100).collect::<String>(), "OpenAIRetryLogic: Not retrying server error");
                    false
                }
            }
            _ => {
                error!(full_error=?error, "OpenAIRetryLogic: Not retrying non-server/transport/timeout error");
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
            warn!(%status, "OpenAIRetryLogic: Instructing to retry due to response status");
            RetryAction::Retry(Cow::Owned(format!(
                "Server responded with status {}",
                status
            )))
        } else if status.is_client_error() { // 4xx errors that are not TOO_MANY_REQUESTS
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
    let file_appender = tracing_appender::rolling::daily(".", LOG_FILE_NAME);
    let (non_blocking_appender, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info").add_directive("rate_limiter_aimd=info".parse().unwrap()));

    let console_layer = fmt::layer().with_writer(std::io::stdout).with_ansi(true).with_target(true).with_level(true);
    let file_layer = fmt::layer().with_writer(non_blocking_appender).with_ansi(false).with_target(true).with_level(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .try_init()?;

    info!("Tracing initialized. Logging to console and {}", LOG_FILE_NAME);
    dotenvy::dotenv().ok();
    info!("Attempted to load .env file");

    let api_key = env::var(OPENAI_API_KEY_ENV_VAR)
        .map_err(|e| {
            error!("Missing OpenAI API key env var: {}. Error: {}", OPENAI_API_KEY_ENV_VAR, e);
            format!("Missing OpenAI API key: {}", OPENAI_API_KEY_ENV_VAR)
        })?;
    let base_url = env::var(OPENAI_BASE_URL_ENV_VAR).unwrap_or_else(|_| DEFAULT_OPENAI_BASE_URL.to_string());
    let chat_completions_url = format!("{}{}", base_url, CHAT_COMPLETIONS_PATH);
    let model_name = env::var(OPENAI_MODEL_NAME_ENV_VAR).unwrap_or_else(|_| DEFAULT_MODEL_NAME.to_string());

    info!(target: "config", api_endpoint = %chat_completions_url, model = %model_name, api_key_prefix = %api_key[..std::cmp::min(8, api_key.len())]);

    let reqwest_client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
    let reqwest_service = ReqwestService::new_with_client(reqwest_client);

    let ac_settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(3) // Can start a bit higher if confident
        .max_concurrency_limit(20) // Max overall concurrency
        .ewma_alpha(0.3)
        .decrease_ratio(0.80) // Be a bit more aggressive in decreasing on backpressure
        .rtt_deviation_scale(2.0)
        .build();
    info!(target: "config", adaptive_concurrency_settings = ?ac_settings);

    let openai_retry_logic = OpenAIRetryLogic::default();

    // Adaptive Concurrency Layer
    let concurrency_layer = AdaptiveConcurrencyLimitLayer::new(
        None,
        ac_settings,
        openai_retry_logic.clone(), // Clone for AC layer
    );

    // Retry Policy for RetryLayer
    let retry_policy = FibonacciRetryPolicy::new(
        MAX_INDIVIDUAL_REQUEST_RETRIES,
        Duration::from_secs(INITIAL_RETRY_BACKOFF_SECS),
        Duration::from_secs(MAX_RETRY_BACKOFF_SECS),
        openai_retry_logic, // Use the same logic for deciding retriability
        JitterMode::Full,
    );
    info!(target: "config", retry_policy_max_attempts = MAX_INDIVIDUAL_REQUEST_RETRIES, initial_backoff_s = INITIAL_RETRY_BACKOFF_SECS, max_backoff_s = MAX_RETRY_BACKOFF_SECS);


    // Build the Tower Service with RetryLayer wrapping AdaptiveConcurrencyLimitLayer
    let mut service = ServiceBuilder::new()
        .layer(RetryLayer::new(retry_policy)) // Retries individual requests with backoff
        .layer(concurrency_layer)             // Manages overall concurrency
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
            // info!("[Task {}] Preparing request to {} with prompt: \"{:.30}...\"", task_id, url_clone, prompt_content);
            let request_payload = json!({
                "model": model_clone, "messages": [{"role": "user", "content": prompt_content}],
                "max_tokens": 50, "temperature": 0.7
            });
            let body_bytes = serde_json::to_vec(&request_payload).map_err(|e| CrateError::from(format!("[T{}] PayloadErr: {}", task_id, e)))?;
            let body = reqwest::Body::from(body_bytes);
            let http_request = HttpRequest::builder().method("POST").uri(url_clone.as_str())
                .header("Authorization", format!("Bearer {}", key_clone)).header("Content-Type", "application/json")
                .body(Some(body)).map_err(|e| CrateError::from(format!("[T{}] BuildReqErr: {}", task_id, e)))?;

            // info!("[Task {}] Waiting for service readiness...", task_id);
            if let Err(e) = cloned_service.ready().await {
                 let err_msg = format!("[T{}] NotReady: {:?}", task_id, e);
                 error!("{}", err_msg); return Err(CrateError::from(err_msg));
            }
            info!("[T{}] Calling service...", task_id);
            match cloned_service.call(http_request).await {
                Ok(response) => {
                    let status = response.status();
                    let response_body_text = response.text().await.unwrap_or_else(|e| format!("ErrReadBody: {}", e));
                    if status.is_success() {
                        info!("[T{}] SUCCESS: {} Body: {:.60}...", task_id, status, response_body_text.trim().replace('\n', " "));
                    } else {
                        warn!("[T{}] API_ERR: {} Body: {}", task_id, status, response_body_text);
                    }
                }
                Err(e) => error!("[T{}] SVC_ERR: {:?}", task_id, e),
            }
            Ok::<(), CrateError>(())
        });
        join_handles.push(handle);
        if REQUEST_INTERVAL_MS > 0 { sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await; }
    }

    for (idx, handle) in join_handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(_)) => info!("[Main] Task {} completed.", idx),
            Ok(Err(e)) => error!("[Main] Task {} failed: {:?}", idx, e),
            Err(e) => error!("[Main] Task {} panicked: {:?}", idx, e),
        }
    }
    info!("All {} prompts processed. Check {} for logs.", NUM_PROMPTS_TO_SEND, LOG_FILE_NAME);
    Ok(())
}