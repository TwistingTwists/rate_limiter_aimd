use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum HttpError {
    // #[snafu(display("Failed to build TLS connector: {}", source))]
    // BuildTlsConnector { source: TlsError },
    // #[snafu(display("Failed to build HTTPS connector: {}", source))]
    // MakeHttpsConnector { source: openssl::error::ErrorStack },
    // #[snafu(display("Failed to build Proxy connector: {}", source))]
    // MakeProxyConnector { source: InvalidUri },
    // #[snafu(display("Failed to make HTTP(S) request: {}", source))]
    // CallRequest { source: hyper::Error },
    // #[snafu(display("Failed to build HTTP request: {}", source))]
    // BuildRequest { source: http::Error },
}

impl HttpError {
    pub const fn is_retriable(&self) -> bool {
        // match self {
            // HttpError::BuildRequest { .. } | HttpError::MakeProxyConnector { .. } => false,
            // HttpError::CallRequest { .. }
            // | HttpError::BuildTlsConnector { .. }
            // | HttpError::MakeHttpsConnector { .. } => true,
        // }
        false
    }
}
