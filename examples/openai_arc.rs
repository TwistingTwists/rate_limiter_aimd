// examples/openai_adaptive_client.rs
use http::{Method, Request, header};
use rate_limiter_aimd::{
    adaptive_concurrency::{
        layer::AdaptiveConcurrencyLimitLayer, reqwest_integration::ReqwestService, retries::{DefaultReqwestRetryLogic, FibonacciRetryPolicy, JitterMode}, service::AdaptiveConcurrencyLimit, AdaptiveConcurrencySettings
    }, Error as CrateError
};
use tower::{
    ServiceBuilder, ServiceExt,
    limit::RateLimit,
    retry::Retry,
    timeout::Timeout,
    Service,
};
use serde::{Deserialize, Serialize};
use std::{env, sync::Arc};
use std::time::Duration;
use tokio::time::Instant;
use tracing::{Level, error, info, warn}; // Added warn and Level
use tracing_subscriber::FmtSubscriber;

// modify these if you want to test with different models
const OPENAI_API_BASE_URL: &str = "https://api.kluster.ai/v1";
const COMPLETIONS_ENDPOINT: &str = "/chat/completions";
const MODEL: &str = "klusterai/Meta-Llama-3.1-8B-Instruct-Turbo";

    // Type alias for our service stack
    type OpenAIService = RateLimit<
        AdaptiveConcurrencyLimit<
            Retry<FibonacciRetryPolicy<DefaultReqwestRetryLogic>, Timeout<ReqwestService>>,
            DefaultReqwestRetryLogic>>;

// Simplified OpenAI request structure
#[derive(Serialize, Debug)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)] // Added Clone for Message
struct Message {
    role: String,
    content: String,
}

// Simplified OpenAI response structure
#[derive(Deserialize, Debug)]
struct OpenAiChatResponse {
    id: String,
    choices: Vec<Choice>,
    // Add other fields as needed, like 'usage'
}

#[derive(Deserialize, Debug)]
struct Choice {
    index: u32,
    message: Message, // Changed from MessageContent to reuse Message
                      // finish_reason: String,
}

// Helper to build the HTTP request for OpenAI
fn build_openai_request(
    api_key: &str,
    prompt: &str,
) -> Result<Request<Option<reqwest::Body>>, CrateError> {
    let request_payload = OpenAiChatRequest {
        model: MODEL.to_string(), // Or another model
        messages: vec![Message {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        max_tokens: 50,
    };

    let body_bytes = serde_json::to_vec(&request_payload).map_err(|e| Box::new(e) as CrateError)?; // Convert serde_json::Error
    let reqwest_body = reqwest::Body::from(body_bytes);

    let uri = format!("{}{}", OPENAI_API_BASE_URL, COMPLETIONS_ENDPOINT);

    let http_request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", api_key))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Some(reqwest_body))
        .map_err(|e| Box::new(e) as CrateError)?; // Convert http::Error

    Ok(http_request)
}

#[tokio::main]
async fn main() -> Result<(), CrateError> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO) // Set to DEBUG for more verbose output from the library
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let openai_api_key = env::var("OPENAI_API_KEY").map_err(|e| Box::new(e) as CrateError)?; // Convert env::VarError

    // 1. Create the base ReqwestService
    let reqwest_service = ReqwestService::new();

    // 2. Define the retry logic (this informs the adaptive controller)
    let retry_logic = DefaultReqwestRetryLogic;

    // Configure settings
    let settings = AdaptiveConcurrencySettings::builder()
        .initial_concurrency(3)
        .max_concurrency_limit(20)
        .build();

    // Build the service stack
    let adaptive_openai_client: OpenAIService = ServiceBuilder::new()
        .rate_limit(10, Duration::from_secs(1)) // 10 requests per second
        .layer(AdaptiveConcurrencyLimitLayer::new(
            None, // Use adaptive behavior
            settings.clone(),
            retry_logic.clone(),
        ))
        .retry(FibonacciRetryPolicy::new(
         5,
         Duration::from_millis(100),
         Duration::from_secs(600),
         retry_logic.clone(),
         JitterMode::Full,
        ))
         .timeout(Duration::from_secs(800))
        .service(reqwest_service);
       
    info!("Adaptive OpenAI client initialized. Sending requests...");
    info!(
        "Initial concurrency: {}, Max concurrency: {}",
        settings.get_initial_concurrency(),
        settings.get_max_concurrency_limit()
    );

    // Create a larger set of prompts to thoroughly test the concurrency limits
    let prompts = vec![
        "Tell me a joke about a programmer.",
        "What is the capital of France?",
        "Explain quantum computing in simple terms.",
        "Write a short poem about the Rust crab.",
        "What are the benefits of adaptive concurrency limiting?",
        "Tell me a fun fact about space.",
        "What is the weather like in London?",
        "Recommend a good sci-fi book.",
        "Who won the world cup in 2022?",
        "Give me a recipe for pancakes.",
        "What is the airspeed velocity of an unladen swallow?",
        "Tell me about the future of AI.",
        "Explain the concept of blockchain.",
        "India Since independence, how has the constitution of india evolved over the years? - 500 words answer",
        "How does photosynthesis work?",
        "List three famous painters and their most known work.",
        "Describe the theory of relativity in simple terms.",
        "What is the difference between artificial intelligence and machine learning?",
        "Explain the concept of cryptocurrency.",
        "Write a haiku about springtime.",
        "What is the largest planet in our solar system?",
        "Tell me about the history of the internet.",
        "What is the difference between a comet and an asteroid?",
        "Explain how a quantum computer works.",
        "What is the significance of the Turing Test?",
        "Describe the process of photosynthesis.",
        "What is the difference between a virus and a bacteria?",
        "Brahmos missile description please.",
        "Indus water treaty description, 30 words.",
        "How far is the Moon from Earth?",
        "What is the tallest mountain in the world?",
        "Name a famous painting by Van Gogh.",
        "What does DNA stand for?",
        "Who invented the lightbulb?",
        "What’s the capital of Brazil?",
        "How many bones are in the human body?",
        "Give me a quick joke.",
        "What is the largest ocean on Earth?",
        "Translate 'hello' into Japanese.",
        "Who wrote *Pride and Prejudice*?",
        "What is the speed of light?",
        "Tell me a riddle.",
        "What’s the population of New York City?",
        "How do black holes form?",
        "Who played Iron Man in the Marvel movies?",
        "What’s a good beginner yoga pose?",
        "Explain gravity like I’m five.",
        "What is Bitcoin?",
        "How do you say 'thank you' in French?",
        "Name a constellation in the night sky.",
        "What is a haiku?",
        "Who painted the Mona Lisa?",
        "Give me a tip for learning a new language.",
        "What is quantum computing?",
        "How do birds navigate during migration?",
        "What’s a good workout for beginners?",
        "Who was the first person in space?",
        "What does a rainbow symbolize?",
        "Describe what happens during an eclipse.",
    ];

    let mut tasks = Vec::new();

    info!("\n\n prompt length = {}\n\n", prompts.len());

    for (i, prompt_text) in prompts.into_iter().enumerate() {
        let request = build_openai_request(&openai_api_key, prompt_text)?;

        // AdaptiveConcurrencyLimit is Clone if its inner service and logic are Clone.
        // ReqwestService and DefaultReqwestRetryLogic are Clone.
        let client_clone = adaptive_openai_client.clone();

        let task_prompt = prompt_text.to_string(); // Clone prompt_text for the async block
        let task = tokio::spawn(async move {
            info!(
                task_id = i,
                "Preparing to send request for: '{}'", task_prompt
            );
            let request_start_time = Instant::now();

            // Wait for the service to be ready (acquires a permit from the adaptive semaphore)
            match client_clone.ready().await {
                Ok(mut ready_client) => {
                    info!(
                        task_id = i,
                        "Service ready, sending request for: '{}'", task_prompt
                    );
                    match ready_client.call(request).await {
                        Ok(response) => {
                            let status = response.status();
                            let rtt = request_start_time.elapsed();
                            if status.is_success() {
                                match response.json::<OpenAiChatResponse>().await {
                                    Ok(chat_response) => {
                                        info!(
                                            task_id = i,
                                            status = %status,
                                            rtt = ?rtt,
                                            prompt = task_prompt,
                                            response = ?chat_response.choices.get(0).map(|c| &c.message.content),
                                            "SUCCESS"
                                        );
                                    }
                                    Err(e) => {
                                        error!(
                                            task_id = i,
                                            status = %status,
                                            rtt = ?rtt,
                                            prompt = task_prompt,
                                            error = %e,
                                            "JSON PARSE ERROR"
                                        );
                                    }
                                }
                            } else {
                                let error_body_text = response
                                    .text()
                                    .await
                                    .unwrap_or_else(|_| "Could not read error body".to_string());
                                warn!( // Changed to warn as it's an API error, not service error
                                    task_id = i,
                                    status = %status,
                                    rtt = ?rtt,
                                    prompt = task_prompt,
                                    error_body = %error_body_text,
                                    "API ERROR"
                                );
                            }
                        }
                        Err(e) => {
                            // This error is CrateError, likely wrapping GenericHttpError
                            let rtt = request_start_time.elapsed();
                            error!(
                                task_id = i,
                                rtt = ?rtt,
                                prompt = task_prompt,
                                error = ?e, // Log the CrateError
                                "SERVICE CALL ERROR"
                            );
                        }
                    }
                }
                Err(e) => {
                    // This error is from poll_ready (CrateError)
                    error!(
                        task_id = i,
                        prompt = task_prompt,
                        error = ?e,
                        "SERVICE NOT READY"
                    );
                }
            }
        });
        tasks.push(task);
        // Stagger initial requests slightly to observe ramp-up.
        // Remove or reduce for more aggressive initial load.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for all spawned tasks to complete
    for task in tasks {
        if let Err(e) = task.await {
            error!("A spawned task panicked or was cancelled: {:?}", e);
        }
    }

    info!("All tasks completed.");
    Ok(())
}
