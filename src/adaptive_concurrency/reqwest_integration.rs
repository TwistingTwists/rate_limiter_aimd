// src/adaptive_concurrency/reqwest_integration.rs
use crate::adaptive_concurrency::http::HttpError as GenericHttpError;
use futures::future::BoxFuture; // Optional: use if converting reqwest::Error to GenericHttpError
use crate::Error as CrateError;
// NOTE: DefaultReqwestRetryLogic would typically be in retries.rs or a dedicated retry_logics.rs
// For this example, assuming it's accessible via `super::retries::DefaultReqwestRetryLogic` if moved.
// Or, if it's specific to reqwest and not generally reusable, it could live here.
// Let's assume it's defined elsewhere (e.g., in retries.rs as previously discussed) for better separation.
// use super::retries::DefaultReqwestRetryLogic;

use http::Request as HttpRequest;
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
            request_future.await.map_err(|e| GenericHttpError::ClientError {
                source: Box::new(e),
            })
        })
    }
}