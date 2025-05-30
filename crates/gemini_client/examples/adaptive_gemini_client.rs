use gemini_client::{
    Content,
    // Updated to gemini_client
    GeminiClient,
    GeminiClientConfig,
    GenerateContentRequest,
    Part,
    // Import other necessary structs like GenerationConfig, SafetySetting if you use them directly
};

use rate_limiter_aimd::adaptive_concurrency::AdaptiveConcurrencySettings;

use std::env;
use std::str::FromStr;
use std::time::Duration;
use tokio;
use tracing::{error, info, warn};
use tracing_subscriber;

// --- Configuration Environment Variable Names ---
const ENV_GEMINI_API_KEY: &str = "GEMINI_API_KEY";
const ENV_GEMINI_BASE_URL: &str = "GEMINI_API_BASE_URL"; // Optional, defaults are in lib.rs
const ENV_GEMINI_DEFAULT_MODEL: &str = "GEMINI_DEFAULT_MODEL"; // Optional
const ENV_USER_AGENT: &str = "GEMINI_CLIENT_USER_AGENT"; // Optional

// Adaptive Concurrency Settings (can remain generic or be prefixed with GEMINI_AC_)
const ENV_AC_INITIAL_CONCURRENCY: &str = "AC_INITIAL_CONCURRENCY";
const ENV_AC_MAX_CONCURRENCY_LIMIT: &str = "AC_MAX_CONCURRENCY_LIMIT";
// Add other AC env vars if you expose them in GeminiClientConfig or use builder defaults

// Retry Settings (can remain generic or be prefixed with GEMINI_RETRY_)
const ENV_RETRY_MAX_ATTEMPTS: &str = "RETRY_MAX_ATTEMPTS";
const ENV_RETRY_INITIAL_BACKOFF_MS: &str = "RETRY_INITIAL_BACKOFF_MS";
const ENV_RETRY_EXP_BASE: &str = "RETRY_EXP_BASE";
const ENV_RETRY_MAX_SINGLE_DELAY_SECS: &str = "RETRY_MAX_SINGLE_DELAY_SECS";

// Helper to parse environment variables with a default
fn get_env_var<T: FromStr + std::fmt::Debug>(var_name: &str, default_value: T) -> T
where
    <T as FromStr>::Err: std::fmt::Debug,
{
    env::var(var_name)
        .ok()
        .and_then(|val_str| match val_str.parse::<T>() {
            Ok(val) => Some(val),
            Err(e) => {
                warn!(
                    "Failed to parse env var '{}' (value: '{}'). Error: {:?}. Using default: {:?}",
                    var_name, val_str, e, default_value
                );
                None
            }
        })
        .unwrap_or(default_value)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let default_log_filter = "info,gemini_client=info,rate_limiter_aimd=info";
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| default_log_filter.to_string()))
        .init();
    info!(
        "Tracing initialized. RUST_LOG or default '{}' is active.",
        default_log_filter
    );

    if dotenvy::dotenv().is_ok() {
        info!(".env file loaded successfully.");
    } else {
        info!(
            "No .env file found or failed to load. Proceeding with environment variables or defaults."
        );
    }

    let api_key = env::var(ENV_GEMINI_API_KEY).map_err(|_| {
        format!(
            "Required environment variable '{}' not set",
            ENV_GEMINI_API_KEY
        )
    })?;

    let default_config = GeminiClientConfig::default();

    let ac_settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(get_env_var(
            ENV_AC_INITIAL_CONCURRENCY,
            default_config
                .adaptive_concurrency
                .get_initial_concurrency(),
        ))
        .max_concurrency_limit(get_env_var(
            ENV_AC_MAX_CONCURRENCY_LIMIT,
            default_config
                .adaptive_concurrency
                .get_max_concurrency_limit(),
        ))
        // Add other AC settings from env if needed
        .build();

    info!(target: "config_loading", ?ac_settings);

    let config = GeminiClientConfig {
        api_key,
        base_url: env::var(ENV_GEMINI_BASE_URL).unwrap_or(default_config.base_url),
        default_model: env::var(ENV_GEMINI_DEFAULT_MODEL).unwrap_or(default_config.default_model),
        user_agent: env::var(ENV_USER_AGENT).ok().or(default_config.user_agent),
        reqwest_client: None,
        adaptive_concurrency: ac_settings,
        retry_max_attempts: get_env_var(ENV_RETRY_MAX_ATTEMPTS, default_config.retry_max_attempts),
        retry_initial_backoff_ms: get_env_var(
            ENV_RETRY_INITIAL_BACKOFF_MS,
            default_config.retry_initial_backoff_ms,
        ),
        retry_exp_base: get_env_var(ENV_RETRY_EXP_BASE, default_config.retry_exp_base),
        retry_max_single_delay_secs: get_env_var(
            ENV_RETRY_MAX_SINGLE_DELAY_SECS,
            default_config.retry_max_single_delay_secs,
        ),
    };

    info!(target: "config_final", client_config = ?config, "GeminiClient configuration loaded.");

    let mut client = GeminiClient::new(config.clone())?; // Clone config if you need it later outside client

    // --- First example call (default model) ---
    let user_prompt1 = "What is the color of the sky on a clear day? Answer concisely.".to_string();
    let default_model_name = client.config.default_model.clone();
    info!(
        "Sending request with default model ('{}') for prompt: '{}'",
        default_model_name, user_prompt1
    );

    match client
        .generate_text_from_user_prompt(&default_model_name, user_prompt1)
        .await
    {
        Ok(text_response) => {
            println!("Assistant (default model): {}", text_response);
        }
        Err(e) => error!("Error with default model: {}", e),
    }

    // --- Second example call (specific model, more complex request) ---
    let model_for_specific_request = client.config.default_model.clone(); // Or specify another, e.g., "gemini-1.5-flash-latest"
    let contents2 = vec![Content {
        role: Some("user".to_string()),
        parts: Some(vec![Part {
            text: Some("Write a short poem about a curious robot.".to_string()),
        }]),
    }];
    let specific_request = GenerateContentRequest {
        contents: contents2,
        generation_config: Some(gemini_client::GenerationConfig {
            temperature: Some(0.7),
            max_output_tokens: Some(100),
            ..Default::default() // Ensure other fields are defaulted if GenerationConfig derives Default
        }),
        safety_settings: None,
    };
    info!(
        "Sending specific request (model: {})...",
        model_for_specific_request
    );
    match client
        .generate_content(&model_for_specific_request, specific_request)
        .await
    {
        Ok(response) => {
            if let Some(candidate) = response.candidates.as_ref().and_then(|c| c.first()) {
                if let Some(content) = &candidate.content {
                    if let Some(part) = content.parts.as_ref().and_then(|p| p.first()) {
                        if let Some(text) = &part.text {
                            println!("Assistant (specific request): {}", text);
                        } else {
                            println!("Assistant (specific request): No text in the first part.");
                        }
                    } else {
                        println!("Assistant (specific request): No parts in content.");
                    }
                } else {
                    println!("Assistant (specific request): No content in candidate.");
                }
            } else {
                println!("Assistant (specific request): No candidates returned.");
            }
        }
        Err(e) => error!("Error with specific model request: {}", e),
    }

    // --- Example of concurrent requests ---
    let num_concurrent_tasks = 10; // Reduced for Gemini free tier, adjust as needed
    info!("Spawning {} concurrent tasks...", num_concurrent_tasks);
    let mut tasks = vec![];
    for i in 0..num_concurrent_tasks {
        let mut client_clone = client.clone(); // Client is Clone
        let task_model = client.config.default_model.clone();
        tasks.push(tokio::spawn(async move {
            let prompt = format!(
                "Briefly explain concept #{} for a five-year-old in one sentence.",
                i
            );
            match client_clone
                .generate_text_from_user_prompt(&task_model, prompt)
                .await
            {
                Ok(res_text) => println!(
                    "[Task {}] SUCCESS: {:.100}...",
                    i,
                    res_text.replace('\n', " ")
                ),
                Err(e) => error!("[Task {}] ERROR: {}", i, e),
            }
        }));
        if i < 5 {
            tokio::time::sleep(Duration::from_millis(200)).await; // Slightly more stagger for Gemini
        }
    }
    for (i, task) in tasks.into_iter().enumerate() {
        match task.await {
            Ok(_) => info!("[Main] Task {} finished.", i),
            Err(e) => error!("[Main] Task {} join error: {}", i, e),
        }
    }

    info!("All example calls completed.");
    Ok(())
}
