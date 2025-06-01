//! Limit the max number of requests being concurrently processed.

mod controller;
mod future;
pub mod layer;
mod semaphore;
pub mod service;

pub mod http;
pub mod internal_event;
pub mod reqwest_integration;
pub mod retries;
pub mod stats; // <-- Add this line

use std::fmt;

pub(crate) use layer::AdaptiveConcurrencyLimitLayer;
pub(crate) use service::AdaptiveConcurrencyLimit;

fn instant_now() -> std::time::Instant {
    tokio::time::Instant::now().into()
}
use bon::Builder;

/// Configuration of adaptive concurrency parameters.
///
/// These parameters typically do not require changes from the default, and incorrect values can lead to meta-stable or
/// unstable performance and sink behavior. Proceed with caution.
// The defaults for these values were chosen after running several simulations on a test service that had
// various responses to load. The values are the best balances found between competing outcomes.
#[derive(Clone, Copy, Debug, Builder)]
// #[serde(deny_unknown_fields)]
/// Configuration settings for the AIMD (Additive Increase/Multiplicative Decrease) 
/// adaptive concurrency control algorithm.
/// 
/// This struct provides various configuration options to tune the behavior of the 
/// adaptive concurrency limiter. The algorithm adjusts the number of concurrent requests
/// based on response latencies and errors, using an AIMD approach similar to TCP congestion control.
/// 
/// # Configuration Parameters
/// 
/// Since all fields are private, configuration must be done through the builder pattern.
/// The following table summarizes available parameters:
/// 
/// | Parameter | Default | Description |
/// |-----------|---------|-------------|
/// | `initial_concurrency` | 1 | Starting number of concurrent requests<br>**Recommendations**: Set to service's average concurrency limit<br>Higher = faster ramp-up but riskier<br>Lower = safer cold-start but underutilized |
/// | `decrease_ratio` | 0.9 | Multiplicative decrease factor on congestion<br>**Range**: 0-1<br>**Trade-offs**:<br>- Higher (0.95) = gentler backoffs<br>- Lower (0.7) = faster congestion recovery |
/// | `ewma_alpha` | 0.4 | Smoothing factor for latency measurements<br>**Formula**: `new_avg = alpha*current + (1-alpha)*prev`<br>**Range**: 0-1<br>**Recommendations**:<br>- Increase for bursty workloads<br>- Decrease for stable services |
/// | `rtt_deviation_scale` | 2.5 | Multiplier for abnormal latency threshold<br>**Formula**: `threshold = avg + scale*deviation`<br>**Range**: ≥0 (1.0-3.0 typical)<br>**Tuning**:<br>- Higher = fewer false positives<br>- Lower = more sensitive to fluctuations |
/// | `max_concurrency_limit` | 200 | Upper bound for concurrency<br>**Considerations**:<br>- Set to service's max safe capacity<br>- Too low = caps performance<br>- Too high = risk of cascading failures |
/// 
/// # Example
/// 
/// ```rust
/// use rate_limiter_aimd::adaptive_concurrency::AdaptiveConcurrencySettings;
/// 
/// // Create settings with custom values
/// let settings = AdaptiveConcurrencySettings::builder()
///     .initial_concurrency(10)
///     .decrease_ratio(0.8)
///     .max_concurrency_limit(500)
///     .build();
/// ```
pub struct AdaptiveConcurrencySettings {
    /// The starting number of concurrent requests allowed.
    ///
    /// This is the concurrency limit when the algorithm starts. The AIMD algorithm will
    /// adjust this value up or down based on observed performance.
    ///
    /// **Default**: 1 (single request at a time)
    ///
    /// # Recommendations
    /// - Set to your service's average concurrency limit if known
    /// - Higher values speed up initial ramp-up but risk overloading new deployments
    /// - Lower values provide safer cold-start but may underutilize resources initially
    #[builder(default)]
    pub(super) initial_concurrency: usize,

    /// Multiplicative decrease factor applied when congestion is detected.
    ///
    /// When error rates exceed thresholds or latency spikes occur, the current concurrency
    /// limit is multiplied by this value to reduce load.
    ///
    /// **Default**: 0.9 (10% decrease)
    /// **Range**: 0 < decrease_ratio < 1
    ///
    /// # Trade-offs
    /// - Higher values (0.95) = gentler backoffs but slower recovery from congestion
    /// - Lower values (0.7) = aggressive backoffs but faster congestion recovery
    #[builder(default)]
    pub(super) decrease_ratio: f64,

    /// Smoothing factor for latency measurements (EWMA).
    ///
    /// Controls how quickly the algorithm forgets old latency measurements:
    /// - Higher alpha = more responsive to recent changes
    /// - Lower alpha = more stable but slower to adapt
    ///
    /// **Default**: 0.4
    /// **Range**: 0 < ewma_alpha < 1
    ///
    /// # Formula
    /// `new_avg = (alpha * current) + ((1 - alpha) * previous_avg)`
    ///
    /// # Recommendations
    /// - Increase for bursty workloads with rapid latency changes
    /// - Decrease for stable services with predictable latency
    #[builder(default)]
    pub(super) ewma_alpha: f64,

    /// Multiplier for determining abnormal latency deviations.
    ///
    /// Used to compute the threshold for latency anomalies:
    /// `threshold = avg_latency + (rtt_deviation_scale * deviation)`
    ///
    /// Responses exceeding this threshold are considered congestion signals.
    ///
    /// **Default**: 2.5
    /// **Range**: ≥0 (typically 1.0-3.0)
    ///
    /// # Tuning
    /// - Higher values = fewer false positives (missed congestion signals)
    /// - Lower values = more sensitive to latency fluctuations (potential false alarms)
    #[builder(default)]
    pub(super) rtt_deviation_scale: f64,

    /// Upper bound for the concurrency limit.
    ///
    /// Prevents unbounded growth that could overwhelm downstream services.
    ///
    /// **Default**: 200
    ///
    /// # Considerations
    /// - Should be set to your service's maximum safe capacity
    /// - Acts as a circuit-breaker for runaway concurrency
    /// - Too low: artificially caps performance
    /// - Too high: risks cascading failures during traffic surges
    #[builder(default)]
    pub(super) max_concurrency_limit: usize,
}

/// Returns the default initial concurrency value (1).
/// 
/// This is used when no custom value is specified in the settings builder.
const fn default_initial_concurrency() -> usize {
    1
}

/// Returns the default decrease ratio (0.9).
/// 
/// This controls how aggressively the concurrency limit is reduced when congestion is detected.
/// A value of 0.9 means the limit will be reduced to 90% of its current value (a 10% decrease).
const fn default_decrease_ratio() -> f64 {
    0.9
}

/// Returns the default EWMA (Exponentially Weighted Moving Average) alpha value (0.4).
/// 
/// This controls the smoothing factor for latency measurements. A higher value gives more weight
/// to recent measurements, while a lower value gives more weight to historical data.
/// 
/// The value should be between 0 and 1. The default of 0.4 provides a balance between
/// responsiveness to recent changes and stability from historical data.
const fn default_ewma_alpha() -> f64 {
    0.4
}

/// Returns the default RTT (Round Trip Time) deviation scale factor (2.5).
/// 
/// This is used to determine when latency variance is high enough to indicate congestion.
/// A higher value makes the algorithm less sensitive to latency variations, while a lower
/// value makes it more sensitive.
const fn default_rtt_deviation_scale() -> f64 {
    2.5
}

/// Returns the default maximum concurrency limit (200).
/// 
/// This sets an upper bound on the concurrency level that the algorithm can reach.
/// It prevents unbounded growth in cases where the system appears to handle higher loads.
const fn default_max_concurrency_limit() -> usize {
    200
}

impl Default for AdaptiveConcurrencySettings {
    fn default() -> Self {
        Self {
            initial_concurrency: default_initial_concurrency(),
            decrease_ratio: default_decrease_ratio(),
            ewma_alpha: default_ewma_alpha(),
            rtt_deviation_scale: default_rtt_deviation_scale(),
            max_concurrency_limit: default_max_concurrency_limit(),
        }
    }
}

impl AdaptiveConcurrencySettings {
    pub fn get_initial_concurrency(&self) -> usize {
        self.initial_concurrency
    }
    pub fn get_max_concurrency_limit(&self) -> usize {
        self.max_concurrency_limit
    }
}

#[derive(Debug)]
pub struct Error(bool);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error")
    }
}

impl std::error::Error for Error {}
