# rate_limiter_aimd

[![Crates.io](https://img.shields.io/crates/v/rate_limiter_aimd)](https://crates.io/crates/rate_limiter_aimd)
[![Docs](https://docs.rs/rate_limiter_aimd/badge.svg)](https://docs.rs/rate_limiter_aimd)

An adaptive concurrency limiter using the AIMD (Additive Increase Multiplicative Decrease) algorithm.

## Overview

This crate implements an adaptive concurrency controller that dynamically adjusts the maximum number of concurrent requests based on observed response times. It uses the AIMD algorithm:

- **Additive Increase**: Gradually increases the concurrency limit when the system is performing well
- **Multiplicative Decrease**: Rapidly decreases the limit when latency increases, errors occur, backpressure is detected, rate limits are exceeded, etc.

The controller helps prevent overloading services by automatically finding the optimal concurrency level.

## Features

- Adaptive concurrency control using AIMD
- Configurable parameters for tuning behavior
- Integration with reqwest HTTP client
- Metrics collection for monitoring performance
- Response classification for backpressure detection
- Automatic retry handling

## Usage

Add to your `Cargo.toml`:
```toml
rate_limiter_aimd = "0.1"
```

Basic example with reqwest:
```rust
use rate_limiter_aimd::adaptive_concurrency::AdaptiveConcurrencyLayer;
use reqwest_middleware::ClientBuilder;

async fn make_requests() {
    let client = reqwest::Client::new();
    let adaptive_layer = AdaptiveConcurrencyLayer::new();
    let client = ClientBuilder::new(client).with(adaptive_layer).build();

    // Use client to make requests (concurrency will be automatically limited)
}
```

## Configuration

Create settings with:
```rust
use rate_limiter_aimd::adaptive_concurrency::AdaptiveConcurrencySettings;

let settings = AdaptiveConcurrencySettings::default()
    .initial_concurrency(10)
    .max_concurrency_limit(100)
    .decrease_ratio(0.8);
```

Available parameters:
- `initial_concurrency`: Starting concurrency limit (default: 1)
- `max_concurrency_limit`: Maximum allowed concurrency (default: 200)
- `decrease_ratio`: Multiplier for decreasing concurrency (default: 0.9)
- `ewma_alpha`: Smoothing factor for latency measurements (default: 0.4)
- `rtt_deviation_scale`: Threshold for latency increases (default: 2.5)

## Response Classification

The controller uses a `ResponseClassifier` to determine if responses indicate backpressure. By default, it classifies:
- HTTP 5xx status codes as backpressure
- HTTP 429 (Too Many Requests) as backpressure
- Response header `x-ratelimit-remaining: 0` as backpressure

Custom classifiers can be implemented.

## Algorithm Details

The controller:
1. Measures request latencies
2. Calculates an exponentially weighted moving average (EWMA) of RTT
3. Decreases concurrency when:
   - Latency exceeds `(1 + rtt_deviation_scale) * EWMA`
   - Response classifier indicates backpressure
   - Errors occur
   - Response contains rate limit exceeded indicators
4. Periodically increases concurrency when no backpressure is detected

## Examples

To run the OpenAI chat example:
```bash
export OPENAI_API_KEY='your-api-key'
cargo run --example openai_chat
```

This demonstrates:
- Setting up an adaptive concurrency client for OpenAI API
- Configuring initial and max concurrency limits
- Sending multiple requests concurrently
- Handling responses with adaptive backpressure control

This work is mostly taken from vector.dev's implementation of adaptive concurrency. Changes lie in `ExponentialPolicy` and `DefaultReqwestRetryLogic`. 

## License

This project is under MPL: Mozilla Public License 2.0

See LICENSE for details.