use openai_client::{ // Or your crate name
    OpenAIClient,
    OpenAIClientConfig,
    ChatMessage,
    ChatCompletionRequest,
};

use rate_limiter_aimd::adaptive_concurrency::AdaptiveConcurrencySettings; // Fix the import to use the correct crate for adaptive_concurrency

use tokio;
use tracing::{info, warn};
use tracing_subscriber;
use std::env;
use std::str::FromStr; // For parsing strings to numbers
use std::time::Duration;

// --- Configuration Environment Variable Names ---
const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
const ENV_OPENAI_BASE_URL: &str = "OPENAI_API_BASE_URL";
const ENV_OPENAI_DEFAULT_MODEL: &str = "OPENAI_DEFAULT_MODEL";
const ENV_USER_AGENT: &str = "OPENAI_CLIENT_USER_AGENT";

// Adaptive Concurrency Settings
const ENV_AC_INITIAL_CONCURRENCY: &str = "AC_INITIAL_CONCURRENCY";
const ENV_AC_MAX_CONCURRENCY_LIMIT: &str = "AC_MAX_CONCURRENCY_LIMIT";
const ENV_AC_DECREASE_RATIO: &str = "AC_DECREASE_RATIO"; // f64
const ENV_AC_EWMA_ALPHA: &str = "AC_EWMA_ALPHA";         // f64
const ENV_AC_RTT_DEVIATION_SCALE: &str = "AC_RTT_DEVIATION_SCALE"; // f64

// Retry Settings
const ENV_RETRY_MAX_ATTEMPTS: &str = "RETRY_MAX_ATTEMPTS";
const ENV_RETRY_INITIAL_BACKOFF_MS: &str = "RETRY_INITIAL_BACKOFF_MS";
const ENV_RETRY_EXP_BASE: &str = "RETRY_EXP_BASE";
const ENV_RETRY_MAX_SINGLE_DELAY_SECS: &str = "RETRY_MAX_SINGLE_DELAY_SECS";

// Helper to parse environment variables with a default
fn get_env_var<T: FromStr + std::fmt::Debug>(var_name: &str, default_value: T) -> T
where
    <T as FromStr>::Err: std::fmt::Debug, // Add Debug bound for error
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
    // Initialize tracing (same as before)
    let default_log_filter = "info,openai_client=info,rate_limiter_aimd=info";
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| default_log_filter.to_string()))
        .init();
    info!("Tracing initialized. RUST_LOG or default '{}' is active.", default_log_filter);

    // Load .env file if present
    if dotenvy::dotenv().is_ok() {
        info!(".env file loaded successfully.");
    } else {
        info!("No .env file found or failed to load. Proceeding with environment variables or defaults.");
    }

    let api_key = env::var(ENV_OPENAI_API_KEY)
        .map_err(|_| format!("Required environment variable '{}' not set", ENV_OPENAI_API_KEY))?;

    // --- Populate OpenAIClientConfig from environment variables or defaults ---
    let default_config = OpenAIClientConfig::default(); // Get default values

    let ac_initial_concurrency = get_env_var(ENV_AC_INITIAL_CONCURRENCY, default_config.adaptive_concurrency.get_initial_concurrency());
    let ac_max_concurrency_limit = get_env_var(ENV_AC_MAX_CONCURRENCY_LIMIT, default_config.adaptive_concurrency.get_max_concurrency_limit());
    // For f64, we need to be careful if default_config.adaptive_concurrency doesn't have direct getters for these
    // Assuming AdaptiveConcurrencySettings has getters or we use its builder's defaults
    let ac_builder_defaults = AdaptiveConcurrencySettings::builder().build(); // to get builder defaults

    let ac_settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(ac_initial_concurrency)
        .max_concurrency_limit(ac_max_concurrency_limit)
        // .decrease_ratio(get_env_var(ENV_AC_DECREASE_RATIO, ac_builder_defaults.decrease_ratio))
        // .ewma_alpha(get_env_var(ENV_AC_EWMA_ALPHA, ac_builder_defaults.ewma_alpha))
        // .rtt_deviation_scale(get_env_var(ENV_AC_RTT_DEVIATION_SCALE, ac_builder_defaults.rtt_deviation_scale))
        .build();
    
    info!(target: "config_loading", ?ac_settings);


    let config = OpenAIClientConfig {
        api_key,
        base_url: env::var(ENV_OPENAI_BASE_URL).unwrap_or(default_config.base_url),
        default_model: env::var(ENV_OPENAI_DEFAULT_MODEL).unwrap_or(default_config.default_model),
        user_agent: env::var(ENV_USER_AGENT).ok().or(default_config.user_agent),
        reqwest_client: None, // Keep as None to use client's default, or add env vars for timeout etc.
        adaptive_concurrency: ac_settings,
        retry_max_attempts: get_env_var(ENV_RETRY_MAX_ATTEMPTS, default_config.retry_max_attempts),
        retry_initial_backoff_ms: get_env_var(ENV_RETRY_INITIAL_BACKOFF_MS, default_config.retry_initial_backoff_ms),
        retry_exp_base: get_env_var(ENV_RETRY_EXP_BASE, default_config.retry_exp_base),
        retry_max_single_delay_secs: get_env_var(ENV_RETRY_MAX_SINGLE_DELAY_SECS, default_config.retry_max_single_delay_secs),
    };

    info!(target: "config_final", client_config = ?config, "OpenAIClient configuration loaded.");


    let mut client = OpenAIClient::new(config)?;

    // --- First example call (default model) ---
    let messages1 = vec![
        ChatMessage { role: "system".to_string(), content: "You are a concise assistant.".to_string()},
        ChatMessage { role: "user".to_string(), content: "What is the color of the sky on a clear day?".to_string()},
    ];
    info!("Sending request with default model...");
    match client.chat_completion_with_messages(messages1.clone()).await {
        Ok(response) => {
            if let Some(choice) = response.choices.first() {
                println!("Assistant (default model): {}", choice.message.content);
            } else {
                println!("Assistant (default model): No choices returned.");
            }
        }
        Err(e) => eprintln!("Error with default model: {}", e),
    }

    // --- Second example call (specific request) ---
    let specific_request = ChatCompletionRequest {
        model: client.config.default_model.clone(), // Using the (potentially env-configured) default model
        messages: messages1, // Re-use messages
        temperature: Some(0.7),
        max_tokens: Some(150),
    };
    info!("Sending specific request (model: {})...", specific_request.model);
    match client.chat_completion(specific_request).await {
        Ok(response) => {
            if let Some(choice) = response.choices.first() {
                println!("Assistant (specific request): {}", choice.message.content);
            } else {
                println!("Assistant (specific request): No choices returned.");
            }
        }
        Err(e) => eprintln!("Error with specific model request: {}", e),
    }
    
    // --- Example of concurrent requests ---
    let num_concurrent_tasks = 20;
    info!("Spawning {} concurrent tasks...", num_concurrent_tasks);
    let mut tasks = vec![];
    for i in 0..num_concurrent_tasks {
        let mut client_clone = client.clone(); // Client is Clone
        tasks.push(tokio::spawn(async move {
            let messages = vec![
                 ChatMessage { role: "user".to_string(), content: format!("Briefly explain concept #{} for a five-year-old in one sentence.", i) },
            ];
            match client_clone.chat_completion_with_messages(messages).await {
                Ok(res) => println!("[Task {}] SUCCESS: {:.80}...",i, res.choices.first().map(|c| c.message.content.replace('\n', " ")).unwrap_or_default()),
                Err(e) => eprintln!("[Task {}] ERROR: {}", i, e),
            }
        }));
        // Stagger spawning slightly to avoid thundering herd on the service.ready() of the very first calls
        // The adaptive limiter will handle the actual concurrency.
        if i < 5 { // Only stagger the first few
             tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    for (i, task) in tasks.into_iter().enumerate() {
        match task.await {
            Ok(_) => info!("[Main] Task {} finished.", i),
            Err(e) => eprintln!("[Main] Task {} join error: {}", i, e),
        }
    }

    info!("All example calls completed.");
    Ok(())
}