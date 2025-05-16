// src/adaptive_concurrency/reqwest_integration.rs
use crate::Error as CrateError;
use crate::adaptive_concurrency::http::HttpError as GenericHttpError;
use futures::future::BoxFuture; // Optional: use if converting reqwest::Error to GenericHttpError
// NOTE: DefaultReqwestRetryLogic would typically be in retries.rs or a dedicated retry_logics.rs
// For this example, assuming it's accessible via `super::retries::DefaultReqwestRetryLogic` if moved.
// Or, if it's specific to reqwest and not generally reusable, it could live here.
// Let's assume it's defined elsewhere (e.g., in retries.rs as previously discussed) for better separation.
// use super::retries::DefaultReqwestRetryLogic;

use http::{Request as HttpRequest, StatusCode};
use reqwest;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::Service;

/// A `tower::Service` wrapper for `reqwest::Client`.
/// Accepts `http::Request<Option<reqwest::Body>>`.
#[derive(Clone)]
pub struct ReqwestService {
    client: reqwest::Client,
}

impl ReqwestService {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub fn new_with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for ReqwestService {
    fn default() -> Self {
        Self::new()
    }
}

impl Service<HttpRequest<Option<reqwest::Body>>> for ReqwestService {
    type Response = reqwest::Response;
    // The error type for this service.
    // GenericHttpError will be converted Into<CrateError>.
    type Error = GenericHttpError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, http_request: HttpRequest<Option<reqwest::Body>>) -> Self::Future {
        let (parts, body_option) = http_request.into_parts();

        let url_str = parts.uri.to_string();
        let url = match reqwest::Url::parse(&url_str) {
            Ok(u) => u,
            Err(parse_err) => {
                let ge = GenericHttpError::InvalidRequest {
                    details: format!("Invalid URL '{}': {}", url_str, parse_err),
                };
                return Box::pin(async move { Err(ge) });
            }
        };

        let mut request_builder = self.client.request(parts.method, url);

        for (header_name, header_value) in parts.headers.iter() {
            request_builder = request_builder.header(header_name, header_value);
        }

        if let Some(body) = body_option {
            request_builder = request_builder.body(body);
        }

        let request_future = request_builder.send();
        Box::pin(async move {
            match request_future.await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        Ok(response)
                    } else {
                        // Get response body for error details
                        let error_body = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Could not read error body".to_string());

                        // Log error with appropriate level based on status
                        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
                            warn!(
                                status = %status,
                                error_body = %error_body,
                                "Server error or rate limited"
                            );
                        } else if status.is_client_error() {
                            error!(
                                status = %status,
                                error_body = %error_body,
                                "Client error"
                            );
                        }

                        Err(GenericHttpError::ServerError {
                            status: status.as_u16(),
                            body: error_body,
                        })
                    }
                }
                Err(e) => {
                    // Handle reqwest errors
                    if e.is_timeout() {
                        warn!(error = %e, "Request timed out");
                        Err(GenericHttpError::Timeout)
                    } else if e.is_connect() {
                        error!(error = %e, "Connection error");
                        Err(GenericHttpError::Transport {
                            source: Box::new(e),
                        })
                    } else {
                        error!(error = %e, "Other reqwest error");
                        Err(GenericHttpError::ClientError {
                            source: Box::new(e),
                        })
                    }
                }
            }
        })
    }
}
