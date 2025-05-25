// src/adaptive_concurrency/http.rs
use crate::Error as CrateError; // Crate-level error type
use snafu::Snafu;

/// A generic error enumeration for HTTP-related issues.
/// Specific client integrations (like Hyper or Reqwest) can define their own
/// errors and provide `From` implementations to convert to this `HttpError`
/// or directly to `CrateError`.
///
/// The `Controller` might look for this specific error type if it needs to
/// make decisions based on "is this an HTTP protocol error vs. a network error".
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum HttpError {
    /// An error occurred during the transport of the request (e.g., network issue, DNS).
    #[snafu(display("HTTP transport error: {}", source))]
    Transport { source: CrateError }, // Can wrap a boxed error from the client

    /// The request itself was malformed or invalid before sending.
    #[snafu(display("Invalid HTTP request: {}", details))]
    InvalidRequest { details: String },

    /// The server responded with an error status code (e.g., 4xx, 5xx)
    /// that is not automatically handled as a retry by the RetryLogic.
    /// This is for cases where the `AdaptiveConcurrencyLimit` might want to know
    /// it was a server-side HTTP error specifically.
    #[snafu(display("HTTP server error response (status {}): {}", status, body))]
    ServerError { status: u16, body: String },

    /// A timeout occurred.
    #[snafu(display("HTTP request timed out"))]
    Timeout,

    /// An error occurred while building the request.
    #[snafu(display("Failed to build HTTP request: {}", details))]
    BuildRequest { details: String },

    /// Other, unspecified HTTP client errors.
    #[snafu(display("Generic HTTP client error: {}", source))]
    ClientError { source: CrateError },
}

// // Implement conversion to the main crate error type.
// // This allows services returning specific HttpErrors (like HyperHttpError or a ReqwestHttpError)
// // to be compatible with AdaptiveConcurrencyLimit which expects S::Error: Into<CrateError>.
// impl From<HttpError> for CrateError {
//     fn from(e: HttpError) -> Self {
//         Box::new(e)
//     }
// }

// --- Example of how a specific integration might use this ---
// In, for example, a hypothetical hyper_integration.rs:
/*
mod hyper_integration {
    use super::HttpError as GenericHttpError; // The one defined above
    use crate::Error as CrateError;
    use hyper;
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    pub enum SpecificHyperError {
        #[snafu(display("Hyper client error: {}", source))]
        Client { source: hyper::Error },
        // other hyper specific errors
    }

    impl From<SpecificHyperError> for GenericHttpError {
        fn from(e: SpecificHyperError) -> Self {
            match e {
                SpecificHyperError::Client { source } => {
                    if source.is_timeout() {
                        GenericHttpError::Timeout
                    } else if source.is_connect() || source.is_incomplete_message() {
                        GenericHttpError::Transport { source: Box::new(source) }
                    } else {
                        GenericHttpError::ClientError { source: Box::new(source) }
                    }
                }
            }
        }
    }

    // And then SpecificHyperError would also implement `From<SpecificHyperError> for CrateError`
    // often via its `From<SpecificHyperError> for GenericHttpError` impl.
    impl From<SpecificHyperError> for CrateError {
        fn from(e: SpecificHyperError) -> Self {
            Box::new(GenericHttpError::from(e))
        }
    }
}
*/

// No specific service implementations here, as those are in their respective
// integration files (e.g., reqwest_integration.rs).
// The primary `tower::Service` trait is the main abstraction for services.
