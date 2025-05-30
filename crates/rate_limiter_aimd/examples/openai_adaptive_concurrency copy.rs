// examples/openai_adaptive_concurrency.rs

use http::{Request as HttpRequest, StatusCode};
use rate_limiter_aimd::{
    Error as CrateError, // The Box<dyn Error...> from lib.rs
    adaptive_concurrency::{
        AdaptiveConcurrencySettings,
        AdaptiveConcurrencySettingsBuilder, // Assuming 'bon::Builder' derives this
        http::HttpError as GenericHttpError,
        layer::AdaptiveConcurrencyLimitLayer,
        reqwest_integration::ReqwestService,
        retries::{RetryAction, RetryLogic},
    },
};
use reqwest::Response as ReqwestResponse;
use std::{borrow::Cow, env, time::Duration};
use tokio::time::sleep;
use tower::{Service, ServiceBuilder, ServiceExt}; // Added ServiceExt for .ready()
use tracing::info;

// --- Configuration ---
const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
const OPENAI_LIST_MODELS_URL: &str = "https://api.kluster.ai/v1/models";
// const OPENAI_LIST_MODELS_URL: &str = "https://api.openai.com/v1/models";
const NUM_CONCURRENT_REQUESTS: usize = 10; // Number of requests to send concurrently
const REQUEST_INTERVAL_MS: u64 = 10; // Interval between launching request tasks

// --- OpenAI Specific Retry Logic ---
#[derive(Clone, Debug, Default)]
struct OpenAIRetryLogic;

impl RetryLogic for OpenAIRetryLogic {
    type Error = GenericHttpError; // Error type from our ReqwestService
    type Response = ReqwestResponse; // Response type from ReqwestService

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
    // --- Setup Tracing (optional, but good for observing limiter behavior) ---
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();

    // --- Load OpenAI API Key ---
    dotenvy::dotenv().ok(); // Load .env file if present
    let api_key = env::var(OPENAI_API_KEY_ENV_VAR).map_err(|_| {
        format!(
            "Missing OpenAI API key environment variable: {}",
            OPENAI_API_KEY_ENV_VAR
        )
    })?;

    // --- Create Reqwest Client and Service ---
    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30)) // Set a timeout for individual requests
        .build()?;
    let reqwest_service = ReqwestService::new_with_client(reqwest_client);

    // --- Configure Adaptive Concurrency ---
    // Use builder if available, otherwise direct struct instantiation
    let ac_settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(2) // Start with a low concurrency
        .max_concurrency_limit(10) // Max concurrent requests client-side
        .ewma_alpha(0.3)
        .decrease_ratio(0.8) // Decrease more gently
        .rtt_deviation_scale(2.0)
        .build();

    // If AdaptiveConcurrencySettingsBuilder is not available (e.g. bon::Builder not fully set up for it)
    // let ac_settings = AdaptiveConcurrencySettings {
    //     initial_concurrency: 2,
    //     max_concurrency_limit: 10,
    //     decrease_ratio: 0.8, // default 0.9
    //     ewma_alpha: 0.3, // default 0.4
    //     rtt_deviation_scale: 2.0, // default 2.5
    // };

    let openai_retry_logic = OpenAIRetryLogic::default();

    // --- Build the Tower Service Stack ---
    // The AdaptiveConcurrencyLimitLayer takes an Option<usize> for fixed concurrency
    // or None for adaptive. We want adaptive.
    let concurrency_layer = AdaptiveConcurrencyLimitLayer::new(
        None, // Use adaptive concurrency, not a fixed limit
        ac_settings,
        openai_retry_logic,
    );

    let mut service = ServiceBuilder::new()
        .layer(concurrency_layer)
        .service(reqwest_service);

    info!("Service initialized. Starting to send requests...");

    // --- Prepare and Send Requests Concurrently ---
    let mut join_handles = Vec::new();

    for i in 0..NUM_CONCURRENT_REQUESTS {
        let mut cloned_service = service.clone(); // Clone service for each task
        let key_clone = api_key.clone();

        let handle = tokio::spawn(async move {
            info!(
                "[Task {}] Preparing request to {}",
                i, OPENAI_LIST_MODELS_URL
            );

            let http_request = HttpRequest::builder()
                .method("GET")
                .uri(OPENAI_LIST_MODELS_URL)
                .header("Authorization", format!("Bearer {}", key_clone))
                .header("Content-Type", "application/json")
                .body(None) // ReqwestService expects Option<reqwest::Body>
                .map_err(|e| CrateError::from(format!("Failed to build request: {}", e)))?;

            info!("[Task {}] Waiting for service readiness...", i);
            // Ensure the service is ready before calling it.
            // This also allows the semaphore in AdaptiveConcurrencyLimit to be acquired.
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
        sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await; // Stagger task spawning a bit
    }

    // Wait for all tasks to complete
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
