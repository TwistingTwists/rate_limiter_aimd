use std::{
    borrow::Cow,
    cmp,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::FutureExt;
use tokio::time::{sleep, Sleep};
use tower::{retry::Policy, timeout::error::Elapsed};
// use vector_lib::configurable::configurable_component; // Assuming this is not needed for this standalone lib

use crate::Error as CrateError; // Changed from `crate::Error` to `crate::Error as CrateError` for clarity
use crate::adaptive_concurrency::http::HttpError as GenericHttpError; // Assuming http.rs exists at this path

use reqwest::{Response as ReqwestResponse, StatusCode};
use tracing::{debug, error, warn}; // Added tracing macros

pub enum RetryAction {
    /// Indicate that this request should be retried with a reason
    Retry(Cow<'static, str>),
    /// Indicate that this request should not be retried with a reason
    DontRetry(Cow<'static, str>),
    /// Indicate that this request should not be retried but the request was successful
    Successful,
}

pub trait RetryLogic: Clone + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    type Response;

    /// When the Service call returns an `Err` response, this function allows
    /// implementors to specify what kinds of errors can be retried.
    fn is_retriable_error(&self, error: &Self::Error) -> bool;

    /// When the Service call returns an `Ok` response, this function allows
    /// implementors to specify additional logic to determine if the success response
    /// is actually an error. This is particularly useful when the downstream service
    /// of a sink returns a transport protocol layer success but error data in the
    /// response body. For example, an HTTP 200 status, but the body of the response
    /// contains a list of errors encountered while processing.
    fn should_retry_response(&self, _response: &Self::Response) -> RetryAction {
        // Treat the default as the request is successful
        RetryAction::Successful
    }
}

/// The jitter mode to use for retry backoff behavior.
#[derive(Clone, Copy, Debug, Default)]
pub enum JitterMode {
    /// No jitter.
    None,

    /// Full jitter.
    ///
    /// The random delay is anywhere from 0 up to the maximum current delay calculated by the backoff
    /// strategy.
    ///
    /// Incorporating full jitter into your backoff strategy can greatly reduce the likelihood
    /// of creating accidental denial of service (DoS) conditions against your own systems when
    /// many clients are recovering from a failure state.
    #[default]
    Full,
}

#[derive(Debug, Clone)]
pub struct FibonacciRetryPolicy<L> {
    remaining_attempts: usize,
    previous_duration: Duration,
    current_duration: Duration,
    jitter_mode: JitterMode,
    current_jitter_duration: Duration,
    max_duration: Duration,
    logic: L,
}

pub struct RetryPolicyFuture<L: RetryLogic> {
    delay: Pin<Box<Sleep>>,
    policy: FibonacciRetryPolicy<L>,
}

impl<L: RetryLogic> FibonacciRetryPolicy<L> {
    pub fn new(
        remaining_attempts: usize,
        initial_backoff: Duration,
        max_duration: Duration,
        logic: L,
        jitter_mode: JitterMode,
    ) -> Self {
        FibonacciRetryPolicy {
            remaining_attempts,
            previous_duration: Duration::from_secs(0),
            current_duration: initial_backoff,
            jitter_mode,
            current_jitter_duration: Self::add_full_jitter(initial_backoff),
            max_duration,
            logic,
        }
    }

    fn add_full_jitter(d: Duration) -> Duration {
        if d.as_millis() == 0 {
            return Duration::from_millis(0); // Avoid panic with modulo by zero
        }
        let jitter = (rand::random::<u64>() % (d.as_millis() as u64)) + 1;
        Duration::from_millis(jitter)
    }

    fn advance(&self) -> FibonacciRetryPolicy<L> {
        let next_duration: Duration = cmp::min(
            self.previous_duration + self.current_duration,
            self.max_duration,
        );

        FibonacciRetryPolicy {
            remaining_attempts: self.remaining_attempts - 1,
            previous_duration: self.current_duration,
            current_duration: next_duration,
            current_jitter_duration: Self::add_full_jitter(next_duration),
            jitter_mode: self.jitter_mode,
            max_duration: self.max_duration,
            logic: self.logic.clone(),
        }
    }

    const fn backoff(&self) -> Duration {
        match self.jitter_mode {
            JitterMode::None => self.current_duration,
            JitterMode::Full => self.current_jitter_duration,
        }
    }

    fn build_retry(&self) -> RetryPolicyFuture<L> {
        let policy = self.advance();
        let delay = Box::pin(sleep(self.backoff()));

        debug!(message = "Retrying request.", delay_ms = %self.backoff().as_millis());
        RetryPolicyFuture { delay, policy }
    }
}

impl<Req, Res, L> Policy<Req, Res, CrateError> for FibonacciRetryPolicy<L>
where
    Req: Clone,
    L: RetryLogic<Response = Res>,
{
    type Future = RetryPolicyFuture<L>;

    // NOTE: in the error cases- `Error` and `EventsDropped` internal events are emitted by the
    // driver, so only need to log here.
    fn retry(&self, _: &Req, result: Result<&Res, &CrateError>) -> Option<Self::Future> {
        match result {
            Ok(response) => match self.logic.should_retry_response(response) {
                RetryAction::Retry(reason) => {
                    if self.remaining_attempts == 0 {
                        error!(
                            message = "OK/retry response but retries exhausted; dropping the request.",
                            reason = ?reason,
                            // internal_log_rate_limit = true, // Assuming this is context specific
                        );
                        return None;
                    }

                    warn!(message = "Retrying after response.", reason = %reason, /* internal_log_rate_limit = true */);
                    Some(self.build_retry())
                }

                RetryAction::DontRetry(reason) => {
                    error!(message = "Not retriable; dropping the request.", reason = ?reason, /* internal_log_rate_limit = true */);
                    None
                }

                RetryAction::Successful => None,
            },
            Err(error) => {
                if self.remaining_attempts == 0 {
                    error!(message = "Retries exhausted; dropping the request.", %error, /* internal_log_rate_limit = true */);
                    return None;
                }

                if let Some(expected) = error.downcast_ref::<L::Error>() {
                    if self.logic.is_retriable_error(expected) {
                        warn!(message = "Retrying after error.", error = %expected, /* internal_log_rate_limit = true */);
                        Some(self.build_retry())
                    } else {
                        error!(
                            message = "Non-retriable error; dropping the request.",
                            %error,
                            // internal_log_rate_limit = true,
                        );
                        None
                    }
                } else if error.downcast_ref::<Elapsed>().is_some() {
                    warn!(
                        message = "Request timed out. If this happens often while the events are actually reaching their destination, try decreasing `batch.max_bytes` and/or using `compression` if applicable. Alternatively `request.timeout_secs` can be increased.",
                        // internal_log_rate_limit = true
                    );
                    Some(self.build_retry())
                } else {
                    error!(
                        message = "Unexpected error type; dropping the request.",
                        %error,
                        // internal_log_rate_limit = true
                    );
                    None
                }
            }
        }
    }

    fn clone_request(&self, request: &Req) -> Option<Req> {
        Some(request.clone())
    }
}

// Safety: `L` is never pinned and we use no unsafe pin projections
// therefore this safe.
impl<L: RetryLogic> Unpin for RetryPolicyFuture<L> {}

impl<L: RetryLogic> Future for RetryPolicyFuture<L> {
    type Output = FibonacciRetryPolicy<L>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        std::task::ready!(self.delay.poll_unpin(cx));
        Poll::Ready(self.policy.clone())
    }
}

impl RetryAction {
    pub const fn is_retryable(&self) -> bool {
        matches!(self, RetryAction::Retry(_))
    }

    pub const fn is_not_retryable(&self) -> bool {
        matches!(self, RetryAction::DontRetry(_))
    }

    pub const fn is_successful(&self) -> bool {
        matches!(self, RetryAction::Successful)
    }
}

/// `DefaultReqwestRetryLogic` provides a default `RetryLogic` implementation
/// for services using `reqwest`.
#[derive(Clone, Debug, Default)]
pub struct DefaultReqwestRetryLogic;

impl RetryLogic for DefaultReqwestRetryLogic {
    type Error = GenericHttpError; // The error type produced by ReqwestService
    type Response = ReqwestResponse; // The success type produced by ReqwestService

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            // Network-level errors are generally retryable
            GenericHttpError::Transport { .. } => true,
            GenericHttpError::Timeout => true,
            // Specific server errors indicated via the error type itself
            GenericHttpError::ServerError { status, .. } => {
                StatusCode::from_u16(*status)
                    .map(|s| s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS)
                    .unwrap_or(false)
            }
            // Errors during request building or client-side processing are typically not retryable
            GenericHttpError::BuildRequest { .. } => false,
            GenericHttpError::InvalidRequest { .. } => false,
            GenericHttpError::ClientError { .. } => false, // e.g., JSON decoding errors
        }
    }

    fn should_retry_response(&self, response: &Self::Response) -> RetryAction {
        let status = response.status();
        if status.is_success() {
            RetryAction::Successful
        } else if status == StatusCode::TOO_MANY_REQUESTS || // Explicit backpressure
                  status == StatusCode::SERVICE_UNAVAILABLE || // Common for temporary issues
                  status.is_server_error() // Other 5xx errors
        {
            RetryAction::Retry(Cow::Owned(format!("Server responded with status {}", status)))
        } else if status.is_client_error() { // 4xx errors that are not TOO_MANY_REQUESTS
            RetryAction::DontRetry(Cow::Owned(format!("Server responded with client error status {}", status)))
        } else {
            // For any other unhandled status codes
            warn!(message = "Unhandled response status for retry logic.", %status);
            RetryAction::DontRetry(Cow::Owned(format!("Server responded with unhandled status {}", status)))
        }
    }
}


// `tokio-retry` crate related code - ExponentialBackoff
// MIT License
// Copyright (c) 2017 Sam Rijs
//
/// A retry strategy driven by exponential back-off.
///
/// The power corresponds to the number of past attempts.
#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    current: u64,
    base: u64,
    factor: u64,
    max_delay: Option<Duration>,
}

impl ExponentialBackoff {
    /// Constructs a new exponential back-off strategy,
    /// given a base duration in milliseconds.
    ///
    /// The resulting duration is calculated by taking the base to the `n`-th power,
    /// where `n` denotes the number of past attempts.
    pub const fn from_millis(base: u64) -> ExponentialBackoff {
        ExponentialBackoff {
            current: base,
            base,
            factor: 1u64,
            max_delay: None,
        }
    }

    /// A multiplicative factor that will be applied to the retry delay.
    ///
    /// For example, using a factor of `1000` will make each delay in units of seconds.
    ///
    /// Default factor is `1`.
    pub const fn factor(mut self, factor: u64) -> ExponentialBackoff {
        self.factor = factor;
        self
    }

    /// Apply a maximum delay. No retry delay will be longer than this `Duration`.
    pub const fn max_delay(mut self, duration: Duration) -> ExponentialBackoff {
        self.max_delay = Some(duration);
        self
    }

    /// Resents the exponential back-off strategy to its initial state.
    pub fn reset(&mut self) {
        self.current = self.base;
    }
}

impl Iterator for ExponentialBackoff {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        // set delay duration by applying factor
        let duration = if let Some(duration) = self.current.checked_mul(self.factor) {
            Duration::from_millis(duration)
        } else {
            Duration::from_millis(u64::MAX)
        };

        // check if we reached max delay
        if let Some(ref max_delay) = self.max_delay {
            if duration > *max_delay {
                return Some(*max_delay);
            }
        }

        if let Some(next) = self.current.checked_mul(self.base) {
            self.current = next;
        } else {
            self.current = u64::MAX;
        }

        Some(duration)
    }
}
