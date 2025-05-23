pub mod adaptive_concurrency;

#[macro_use]
extern crate tracing;
#[macro_use]
extern crate serde_json; // For example payload

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
