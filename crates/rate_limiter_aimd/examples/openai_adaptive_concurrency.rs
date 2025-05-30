// examples/openai_adaptive_concurrency.rs

use http::{Request as HttpRequest, StatusCode};
use rate_limiter_aimd::{
    Error as CrateError,
    adaptive_concurrency::{
        AdaptiveConcurrencySettings,
        http::HttpError as GenericHttpError,
        layer::AdaptiveConcurrencyLimitLayer,
        reqwest_integration::ReqwestService,
        retries::{RetryAction, RetryLogic},
    },
};
use reqwest::Response as ReqwestResponse;
use std::{borrow::Cow, env, time::Duration};
use tokio::time::sleep;
use tower::{Service, ServiceBuilder, ServiceExt};
use tracing::info;

// --- Configuration ---
const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
const OPENAI_BASE_URL_ENV_VAR: &str = "OPENAI_API_BASE_URL"; // New environment variable
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com"; // Default OpenAI URL
const LIST_MODELS_PATH: &str = "/v1/models"; // API path remains the same relative to base

const NUM_CONCURRENT_REQUESTS: usize = 20;
const REQUEST_INTERVAL_MS: u64 = 50;

// --- OpenAI Specific Retry Logic (remains the same as before) ---
#[derive(Clone, Debug, Default)]
struct OpenAIRetryLogic;

impl RetryLogic for OpenAIRetryLogic {
    type Error = GenericHttpError;
    type Response = ReqwestResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            GenericHttpError::Transport { .. } => {
                info!(
                    "OpenAIRetryLogic: Retrying due to transport error: {:?}",
                    error
                );
                true
            }
            GenericHttpError::Timeout => {
                info!(
                    "OpenAIRetryLogic: Retrying due to timeout error: {:?}",
                    error
                );
                true
            }
            GenericHttpError::ServerError { status, .. } => {
                let s = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS {
                    info!(
                        "OpenAIRetryLogic: Retrying due to server error status {}: {:?}",
                        status, error
                    );
                    true
                } else {
                    info!(
                        "OpenAIRetryLogic: Not retrying server error status {}: {:?}",
                        status, error
                    );
                    false
                }
            }
            _ => {
                info!("OpenAIRetryLogic: Not retrying error: {:?}", error);
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
            info!("OpenAIRetryLogic: Retrying due to status: {}", status);
            RetryAction::Retry(Cow::Owned(format!(
                "Server responded with status {}",
                status
            )))
        } else if status.is_client_error() {
            info!(
                "OpenAIRetryLogic: Not retrying due to client error status: {}",
                status
            );
            RetryAction::DontRetry(Cow::Owned(format!(
                "Server responded with client error status {}",
                status
            )))
        } else {
            info!(
                "OpenAIRetryLogic: Not retrying due to unhandled status: {}",
                status
            );
            RetryAction::DontRetry(Cow::Owned(format!(
                "Server responded with unhandled status {}",
                status
            )))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();

    dotenvy::dotenv().ok();
    let api_key = env::var(OPENAI_API_KEY_ENV_VAR).map_err(|_| {
        format!(
            "Missing OpenAI API key environment variable: {}",
            OPENAI_API_KEY_ENV_VAR
        )
    })?;

    // --- Get Base URL for the API ---
    let base_url =
        env::var(OPENAI_BASE_URL_ENV_VAR).unwrap_or_else(|_| DEFAULT_OPENAI_BASE_URL.to_string());
    let list_models_url = format!("{}{}", base_url, LIST_MODELS_PATH); // Construct the full URL

    info!("Using API endpoint: {}", list_models_url);
    info!(
        "Using API key: {}...",
        &api_key[..std::cmp::min(8, api_key.len())]
    );

    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let reqwest_service = ReqwestService::new_with_client(reqwest_client);

    let ac_settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(2)
        .max_concurrency_limit(10)
        .ewma_alpha(0.3)
        .decrease_ratio(0.8)
        .rtt_deviation_scale(2.0)
        .build();

    let openai_retry_logic = OpenAIRetryLogic::default();

    let concurrency_layer =
        AdaptiveConcurrencyLimitLayer::new(None, ac_settings, openai_retry_logic);

    let mut service = ServiceBuilder::new()
        .layer(concurrency_layer)
        .service(reqwest_service);

    info!("Service initialized. Starting to send requests...");

    let mut join_handles = Vec::new();

    for i in 0..NUM_CONCURRENT_REQUESTS {
        let mut cloned_service = service.clone();
        let key_clone = api_key.clone();
        let url_clone = list_models_url.clone(); // Clone the full URL for the task

        let handle = tokio::spawn(async move {
            info!("[Task {}] Preparing request to {}", i, url_clone);

            let http_request = HttpRequest::builder()
                .method("GET")
                .uri(url_clone.as_str()) // Use the constructed URL
                .header("Authorization", format!("Bearer {}", key_clone))
                .header("Content-Type", "application/json")
                .body(None)
                .map_err(|e| CrateError::from(format!("Failed to build request: {}", e)))?;

            info!("[Task {}] Waiting for service readiness...", i);
            cloned_service.ready().await.map_err(|e: CrateError| {
                CrateError::from(format!("[Task {}] Service not ready: {:?}", i, e))
            })?;

            info!("[Task {}] Calling service...", i);
            match cloned_service.call(http_request).await {
                Ok(response) => {
                    let status = response.status();
                    let response_body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| String::from("Error reading response body"));
                    if status.is_success() {
                        info!(
                            "[Task {}] SUCCESS: Status {}, Body: {:.100}...",
                            i, status, response_body
                        );
                    } else {
                        info!(
                            "[Task {}] API_ERROR: Status {}, Body: {}",
                            i, status, response_body
                        );
                    }
                }
                Err(e) => {
                    info!("[Task {}] SERVICE_ERROR: {:?}", i, e);
                }
            }
            Ok::<(), CrateError>(())
        });
        join_handles.push(handle);
        sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await;
    }

    for (idx, handle) in join_handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(_)) => info!("[Main] Task {} completed successfully.", idx),
            Ok(Err(e)) => info!("[Main] Task {} completed with an error: {:?}", idx, e),
            Err(e) => info!("[Main] Task {} panicked: {:?}", idx, e),
        }
    }

    info!("All requests processed.");
    Ok(())
}
