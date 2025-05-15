pub mod adaptive_concurrency;

#[macro_use]
extern crate tracing;


pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
