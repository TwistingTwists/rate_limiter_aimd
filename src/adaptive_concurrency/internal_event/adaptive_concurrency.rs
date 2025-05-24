use std::time::Duration;
use crate::registered_event;
use metrics::{Histogram, histogram};
use tracing;

#[derive(Clone, Copy, Debug)]
pub struct AdaptiveConcurrencyLimitData {
    pub concurrency: u64,
    pub reached_limit: bool,
    pub had_back_pressure: bool,
    pub current_rtt: Option<Duration>,
    pub past_rtt: Duration,
    pub past_rtt_deviation: Duration,
}

registered_event! {
    AdaptiveConcurrencyLimit => {
        // These are histograms, as they may have a number of different
        // values over each reporting interval, and each of those values
        // is valuable for diagnosis.
        limit: Histogram = histogram!("adaptive_concurrency_limit"),
        reached_limit: Histogram = histogram!("adaptive_concurrency_reached_limit"),
        back_pressure: Histogram = histogram!("adaptive_concurrency_back_pressure"),
        past_rtt_mean: Histogram = histogram!("adaptive_concurrency_past_rtt_mean"),
    }

    fn emit(&self, data: AdaptiveConcurrencyLimitData) {
        self.limit.record(data.concurrency as f64);
        let reached_limit_val = data.reached_limit.then_some(1.0).unwrap_or_default();
        self.reached_limit.record(reached_limit_val);
        let back_pressure_val = data.had_back_pressure.then_some(1.0).unwrap_or_default();
        self.back_pressure.record(back_pressure_val);
        self.past_rtt_mean.record(data.past_rtt);

        // ADDED LOGGING FOR THE EXAMPLE
        tracing::info!(
            target: "adaptive_concurrency::stats",
            concurrency_limit = data.concurrency,
            reached_max_limit_this_cycle = data.reached_limit, // Renamed for clarity from "reached_limit"
            had_back_pressure_this_cycle = data.had_back_pressure, // Renamed for clarity
            current_rtt_ms = data.current_rtt.map(|d| d.as_millis()),
            past_rtt_ms = data.past_rtt.as_millis(),
            past_rtt_deviation_ms = data.past_rtt_deviation.as_millis(),
            "Limit Adjusted"
        );
    }
}

registered_event! {
    AdaptiveConcurrencyInFlight => {
        in_flight: Histogram = histogram!("adaptive_concurrency_in_flight"),
    }

    fn emit(&self, in_flight_count: u64) {
        self.in_flight.record(in_flight_count as f64);
        // ADDED LOGGING FOR THE EXAMPLE
        tracing::debug!(target: "adaptive_concurrency::stats", in_flight = in_flight_count, "In-flight Updated");
    }
}

registered_event! {
    AdaptiveConcurrencyObservedRtt => {
        observed_rtt: Histogram = histogram!("adaptive_concurrency_observed_rtt"),
    }

    fn emit(&self, rtt: Duration) {
        self.observed_rtt.record(rtt);
        // Optionally log raw RTTs, can be very verbose
        tracing::trace!(target: "adaptive_concurrency::stats", observed_rtt_ms = rtt.as_millis(), "RTT Observed");
    }
}

registered_event! {
    AdaptiveConcurrencyAveragedRtt => {
        averaged_rtt: Histogram = histogram!("adaptive_concurrency_averaged_rtt"),
    }

    fn emit(&self, rtt: Duration) {
        self.averaged_rtt.record(rtt);
        // Log averaged RTT when it's computed for an adjustment period
        tracing::debug!(target: "adaptive_concurrency::stats", averaged_rtt_ms = rtt.as_millis(), "RTT Averaged for Period");
    }
}
