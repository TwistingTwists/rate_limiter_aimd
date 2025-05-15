# Adaptive Concurrency Control Library

This Rust library provides adaptive concurrency control for network services, implementing an algorithm that automatically adjusts the maximum number of concurrent requests based on observed response times and backpressure signals.

## Features

- Adaptive concurrency limiting based on RTT measurements
- Automatic scaling of concurrency limits
- Integration with Reqwest for HTTP services
- Configurable parameters for fine-tuning behavior
- Support for retry logic and backpressure detection

## Example Usage with OpenAI API

To run the OpenAI example:

1. Set your Kluster AI API key:
```bash
export OPENAI_API_KEY='your-klusterai-api-key'
```

2. Build and run the example:
```bash
cargo run --example openai_arc
```

The example demonstrates:
- Setting up an adaptive concurrency client for OpenAI API
- Configuring initial and max concurrency limits
- Sending multiple requests concurrently
- Handling responses with adaptive backpressure control
