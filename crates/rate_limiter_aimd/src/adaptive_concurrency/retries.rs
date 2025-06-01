use std::{
    borrow::Cow,
    cmp,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::FutureExt;
use http::StatusCode;
use tokio::time::{Sleep, sleep};
use tower::{retry::Policy, timeout::error::Elapsed};
// use vector_lib::configurable::configurable_component;

use crate::{Error, adaptive_concurrency::http::HttpError};

pub enum RetryAction {
    /// Indicate that this request should be retried with a reason
    Retry(Cow<'static, str>),
    /// Indicate that this request should not be retried with a reason
    DontRetry(Cow<'static, str>),
    /// Indicate that this request should not be retried but the request was successful
    Successful,
}

/// Defines the contract for determining which requests should be retried.
/// 
/// Implementers must specify:
/// - Which error types are retriable
/// - Whether a successful response requires additional retries
/// 
/// # Example
/// ```rust
/// use std::error::Error;
/// use rate_limiter_aimd::adaptive_concurrency::retries::RetryLogic;
/// 
/// #[derive(Clone)]
/// struct MyRetryLogic;
/// 
/// impl RetryLogic for MyRetryLogic {
///     type Error = Box<dyn Error + Send + Sync + 'static>;
///     type Response = String;
/// 
///     fn is_retriable_error(&self, error: &Self::Error) -> bool {
///         // Retry on timeout errors
///         error.to_string().contains("timeout")
///     }
/// 
///     fn should_retry_response(&self, response: &Self::Response) -> bool {
///         // Retry if response contains specific retry token
///         response.contains("retry-me")
///     }
/// }
/// ```
pub trait RetryLogic: Clone + Send + Sync + 'static {
    /// The type of errors produced by the service
    type Error: std::error::Error + Send + Sync + 'static;
    
    /// The type of successful responses from the service
    type Response;

    /// Determines if an error should trigger a retry.
    /// 
    /// # Parameters
    /// - `error`: The error that occurred during the service call
    /// 
    /// # Returns
    /// `true` if the error is retriable, `false` otherwise
    fn is_retriable_error(&self, error: &Self::Error) -> bool;

    /// Determines if a successful response should trigger a retry.
    /// 
    /// # Parameters
    /// - `response`: The successful response from the service
    /// 
    /// # Returns
    /// A `RetryAction` indicating whether the response should be retried
    /// 
    /// # Note
    /// This is optional and defaults to `RetryAction::Successful` for backward compatibility.
    /// Implementers should override this only when success responses can indicate retry needs.
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

impl<Req, Res, L> Policy<Req, Res, Error> for FibonacciRetryPolicy<L>
where
    Req: Clone,
    L: RetryLogic<Response = Res>,
{
    type Future = RetryPolicyFuture<L>;

    // NOTE: in the error cases- `Error` and `EventsDropped` internal events are emitted by the
    // driver, so only need to log here.
    fn retry(&self, _: &Req, result: Result<&Res, &Error>) -> Option<Self::Future> {
        match result {
            Ok(response) => match self.logic.should_retry_response(response) {
                RetryAction::Retry(reason) => {
                    if self.remaining_attempts == 0 {
                        error!(
                            message = "OK/retry response but retries exhausted; dropping the request.",
                            reason = ?reason,
                            internal_log_rate_limit = true,
                        );
                        return None;
                    }

                    warn!(message = "Retrying after response.", reason = %reason, internal_log_rate_limit = true);
                    Some(self.build_retry())
                }

                RetryAction::DontRetry(reason) => {
                    error!(message = "Not retriable; dropping the request.", reason = ?reason, internal_log_rate_limit = true);
                    None
                }

                RetryAction::Successful => None,
            },
            Err(error) => {
                if self.remaining_attempts == 0 {
                    error!(message = "Retries exhausted; dropping the request.", %error, internal_log_rate_limit = true);
                    return None;
                }

                if let Some(expected) = error.downcast_ref::<L::Error>() {
                    if self.logic.is_retriable_error(expected) {
                        warn!(message = "Retrying after error.", error = %expected, internal_log_rate_limit = true);
                        Some(self.build_retry())
                    } else {
                        error!(
                            message = "Non-retriable error; dropping the request.",
                            %error,
                            internal_log_rate_limit = true,
                        );
                        None
                    }
                } else if error.downcast_ref::<Elapsed>().is_some() {
                    warn!(
                        message = "Request timed out. If this happens often while the events are actually reaching their destination, try decreasing `batch.max_bytes` and/or using `compression` if applicable. Alternatively `request.timeout_secs` can be increased.",
                        internal_log_rate_limit = true
                    );
                    Some(self.build_retry())
                } else {
                    error!(
                        message = "Unexpected error type; dropping the request.",
                        %error,
                        internal_log_rate_limit = true
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

// `tokio-retry` crate
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
    pub fn new(base: u64, factor: u64, max_delay: Option<Duration>) -> ExponentialBackoff {
        ExponentialBackoff {
            current: base,
            base,
            factor,
            max_delay,
        }
    }

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

// --- NEW: ExponentialBackoffPolicy ---

#[derive(Clone, Debug)]
pub struct ExponentialBackoffPolicy<L: RetryLogic> {
    attempts_remaining: usize,
    backoff_iterator: ExponentialBackoff,
    max_total_retry_duration: Option<Duration>, // Optional: if you want to cap total time across retries
    first_attempt_made: bool, // To ensure backoff_iterator.next() isn't called before the first retry
    logic: L,
    jitter_mode: JitterMode,
}

// Future for ExponentialBackoffPolicy
pub struct ExponentialPolicyFuture<L: RetryLogic> {
    delay: Pin<Box<Sleep>>,
    policy_state_after_delay: ExponentialBackoffPolicy<L>,
}

impl<L: RetryLogic> ExponentialBackoffPolicy<L> {
    pub fn new(
        max_attempts: usize,
        initial_backoff_iterator: ExponentialBackoff, // Already configured with initial, base, max_single_delay
        logic: L,
        jitter_mode: JitterMode,
        max_total_retry_duration: Option<Duration>,
    ) -> Self {
        Self {
            attempts_remaining: max_attempts,
            backoff_iterator: initial_backoff_iterator,
            max_total_retry_duration, // Not directly used by tower-retry's Policy, but useful for configuration
            first_attempt_made: false,
            logic,
            jitter_mode,
        }
    }

    fn build_retry_future(&self, delay_duration: Duration) -> ExponentialPolicyFuture<L> {
        let mut next_policy_state = self.clone();
        next_policy_state.attempts_remaining -= 1;
        next_policy_state.first_attempt_made = true;
        // The iterator for backoff_iterator itself is advanced when .next() is called to get delay_duration

        debug!(
            message = "Retrying request with exponential backoff.",
            delay_ms = %delay_duration.as_millis(),
            attempts_remaining = next_policy_state.attempts_remaining
        );

        ExponentialPolicyFuture {
            delay: Box::pin(sleep(delay_duration)),
            policy_state_after_delay: next_policy_state,
        }
    }

    fn apply_jitter(&self, base_duration: Duration) -> Duration {
        match self.jitter_mode {
            JitterMode::None => base_duration,
            JitterMode::Full => {
                if base_duration.as_millis() == 0 {
                    return Duration::from_millis(0);
                }
                // rand::random generates a value between 0.0 and 1.0
                // Multiply by base_duration.as_millis() to get a random portion of the duration
                let random_millis =
                    (rand::random::<f64>() * base_duration.as_millis() as f64) as u64;
                Duration::from_millis(random_millis)
            }
        }
    }
}

impl<Req, Res, L> Policy<Req, Res, L::Error> for ExponentialBackoffPolicy<L>
where
    Req: Clone,
    L: RetryLogic<Response = Res>, // L::Error requires std::error::Error
{
    type Future = ExponentialPolicyFuture<L>;

    fn retry(&self, _request: &Req, result: Result<&Res, &L::Error>) -> Option<Self::Future> {
        if self.attempts_remaining == 0 {
            error!(message = "Max retry attempts reached; dropping request.", error_if_any=?result.err());
            return None;
        }

        let should_retry_action = match result {
            Ok(response) => self.logic.should_retry_response(response),
            Err(service_error) => {
                if self.logic.is_retriable_error(service_error) {
                    RetryAction::Retry(Cow::Borrowed("Service error deemed retriable"))
                } else {
                    RetryAction::DontRetry(Cow::Borrowed("Service error deemed not retriable"))
                }
            }
        };

        match should_retry_action {
            RetryAction::Retry(reason) => {
                let mut current_backoff_iterator = self.backoff_iterator.clone(); // Clone to call next()
                let base_delay = match current_backoff_iterator.next() {
                    Some(delay) => delay,
                    None => {
                        // Iterator exhausted (e.g., if it had a max number of internal steps)
                        warn!(message = "Exponential backoff iterator exhausted, but attempts remain. Not retrying.", %reason);
                        return None;
                    }
                };
                let jittered_delay = self.apply_jitter(base_delay);

                warn!(message = "Retrying after response/error indicated retry needed.", %reason, base_delay_ms = base_delay.as_millis(), jittered_delay_ms = jittered_delay.as_millis());

                let mut next_policy_state = self.clone();
                next_policy_state.attempts_remaining = self.attempts_remaining.saturating_sub(1);
                next_policy_state.backoff_iterator = current_backoff_iterator; // Store the advanced iterator

                Some(ExponentialPolicyFuture {
                    delay: Box::pin(sleep(jittered_delay)),
                    policy_state_after_delay: next_policy_state,
                })
            }
            RetryAction::DontRetry(reason) => {
                error!(message = "Not retriable (from logic); dropping request.", %reason, error_if_any=?result.err());
                None
            }
            RetryAction::Successful => None,
        }
    }

    fn clone_request(&self, request: &Req) -> Option<Req> {
        Some(request.clone())
    }
}

// Safety: L is never pinned, and we use no unsafe pin projections.
impl<L: RetryLogic> Unpin for ExponentialPolicyFuture<L> {}

impl<L: RetryLogic> Future for ExponentialPolicyFuture<L> {
    type Output = ExponentialBackoffPolicy<L>; // Return the next state of the policy

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        std::task::ready!(self.delay.poll_unpin(cx));
        Poll::Ready(self.policy_state_after_delay.clone())
    }
}

/// `DefaultReqwestRetryLogic` provides a default `RetryLogic` implementation
/// for services using `reqwest`.
#[derive(Clone, Debug, Default)]
pub struct DefaultReqwestRetryLogic;

impl RetryLogic for DefaultReqwestRetryLogic {
    type Error = HttpError; // The error type produced by ReqwestService
    type Response = HttpError; // The success type produced by ReqwestService

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            // Network-level errors are generally retryable
            HttpError::Transport { .. } => true,
            HttpError::Timeout => true,
            // Specific server errors indicated via the error type itself
            HttpError::ServerError { status, .. } => StatusCode::from_u16(*status)
                .map(|s| s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS)
                .unwrap_or(false),
            // Errors during request building or client-side processing are typically not retryable
            HttpError::BuildRequest { .. } => false,
            HttpError::InvalidRequest { .. } => false,
            HttpError::ClientError { .. } => false, // e.g., JSON decoding errors
        }
    }

    fn should_retry_response(&self, error: &Self::Response) -> RetryAction {
        match error {
            HttpError::Transport { .. } => RetryAction::Retry(Cow::Borrowed("Transport error")),
            HttpError::Timeout => RetryAction::Retry(Cow::Borrowed("Request timed out")),
            HttpError::ServerError { status, body } => {
                if let Ok(status) = StatusCode::from_u16(*status) {
                    if status == StatusCode::TOO_MANY_REQUESTS || 
                       status == StatusCode::SERVICE_UNAVAILABLE ||
                       status.is_server_error() {
                        RetryAction::Retry(Cow::Owned(format!(
                            "Server error status {}: {}",
                            status, body
                        )))
                    } else if status.is_client_error() {
                        RetryAction::DontRetry(Cow::Owned(format!(
                            "Client error status {}: {}",
                            status, body
                        )))
                    } else {
                        RetryAction::DontRetry(Cow::Owned(format!(
                            "Unexpected status {}: {}",
                            status, body
                        )))
                    }
                } else {
                    RetryAction::DontRetry(Cow::Owned(format!(
                        "Invalid status code {}",
                        status
                    )))
                }
            },
            HttpError::BuildRequest { details } => RetryAction::DontRetry(
                Cow::Owned(format!("Request build error: {}", details))
            ),
            HttpError::InvalidRequest { details } => RetryAction::DontRetry(
                Cow::Owned(format!("Invalid request: {}", details))
            ),
            HttpError::ClientError { source } => RetryAction::DontRetry(
                Cow::Owned(format!("Client error: {}", source))
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, time::Duration};

    use tokio::time;
    use tokio_test::{assert_pending, assert_ready_err, assert_ready_ok, task};
    use tower::retry::RetryLayer;
    use tower_test::{assert_request_eq, mock};

    use super::*;
    // use crate::test_util::trace_init;

    #[tokio::test]
    async fn service_error_retry() {
        // trace_init();

        time::pause();

        let policy = FibonacciRetryPolicy::new(
            5,
            Duration::from_secs(1),
            Duration::from_secs(10),
            SvcRetryLogic,
            JitterMode::None,
        );

        let (mut svc, mut handle) = mock::spawn_layer(RetryLayer::new(policy));

        assert_ready_ok!(svc.poll_ready());

        let fut = svc.call("hello");
        let mut fut = task::spawn(fut);

        assert_request_eq!(handle, "hello").send_error(Error(true));

        assert_pending!(fut.poll());

        time::advance(Duration::from_secs(2)).await;
        assert_pending!(fut.poll());

        assert_request_eq!(handle, "hello").send_response("world");
        assert_eq!(fut.await.unwrap(), "world");
    }

    #[tokio::test]
    async fn service_error_no_retry() {
        // trace_init();

        let policy = FibonacciRetryPolicy::new(
            5,
            Duration::from_secs(1),
            Duration::from_secs(10),
            SvcRetryLogic,
            JitterMode::None,
        );

        let (mut svc, mut handle) = mock::spawn_layer(RetryLayer::new(policy));

        assert_ready_ok!(svc.poll_ready());

        let mut fut = task::spawn(svc.call("hello"));
        assert_request_eq!(handle, "hello").send_error(Error(false));
        assert_ready_err!(fut.poll());
    }

    #[tokio::test]
    async fn timeout_error() {
        // trace_init();

        time::pause();

        let policy = FibonacciRetryPolicy::new(
            5,
            Duration::from_secs(1),
            Duration::from_secs(10),
            SvcRetryLogic,
            JitterMode::None,
        );

        let (mut svc, mut handle) = mock::spawn_layer(RetryLayer::new(policy));

        assert_ready_ok!(svc.poll_ready());

        let mut fut = task::spawn(svc.call("hello"));
        assert_request_eq!(handle, "hello").send_error(Elapsed::new());
        assert_pending!(fut.poll());

        time::advance(Duration::from_secs(2)).await;
        assert_pending!(fut.poll());

        assert_request_eq!(handle, "hello").send_response("world");
        assert_eq!(fut.await.unwrap(), "world");
    }

    #[test]
    fn backoff_grows_to_max() {
        let mut policy = FibonacciRetryPolicy::new(
            10,
            Duration::from_secs(1),
            Duration::from_secs(10),
            SvcRetryLogic,
            JitterMode::None,
        );
        assert_eq!(Duration::from_secs(1), policy.backoff());

        policy = policy.advance();
        assert_eq!(Duration::from_secs(1), policy.backoff());

        policy = policy.advance();
        assert_eq!(Duration::from_secs(2), policy.backoff());

        policy = policy.advance();
        assert_eq!(Duration::from_secs(3), policy.backoff());

        policy = policy.advance();
        assert_eq!(Duration::from_secs(5), policy.backoff());

        policy = policy.advance();
        assert_eq!(Duration::from_secs(8), policy.backoff());

        policy = policy.advance();
        assert_eq!(Duration::from_secs(10), policy.backoff());

        policy = policy.advance();
        assert_eq!(Duration::from_secs(10), policy.backoff());
    }

    #[test]
    fn backoff_grows_to_max_with_jitter() {
        let max_duration = Duration::from_secs(10);
        let mut policy = FibonacciRetryPolicy::new(
            10,
            Duration::from_secs(1),
            max_duration,
            SvcRetryLogic,
            JitterMode::Full,
        );

        let expected_fib = [1, 1, 2, 3, 5, 8];

        for (i, &exp_fib_secs) in expected_fib.iter().enumerate() {
            let backoff = policy.backoff();
            let upper_bound = Duration::from_secs(exp_fib_secs);

            // Check if the backoff is within the expected range, considering the jitter
            assert!(
                !backoff.is_zero() && backoff <= upper_bound,
                "Attempt {}: Expected backoff to be within 0 and {:?}, got {:?}",
                i + 1,
                upper_bound,
                backoff
            );

            policy = policy.advance();
        }

        // Once the max backoff is reached, it should not exceed the max backoff.
        for _ in 0..4 {
            let backoff = policy.backoff();
            assert!(
                !backoff.is_zero() && backoff <= max_duration,
                "Expected backoff to not exceed {:?}, got {:?}",
                max_duration,
                backoff
            );

            policy = policy.advance();
        }
    }

    #[derive(Debug, Clone)]
    struct SvcRetryLogic;

    impl RetryLogic for SvcRetryLogic {
        type Error = Error;
        type Response = &'static str;

        fn is_retriable_error(&self, error: &Self::Error) -> bool {
            error.0
        }
    }

    #[derive(Debug)]
    struct Error(bool);

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "error")
        }
    }

    impl std::error::Error for Error {}
}
