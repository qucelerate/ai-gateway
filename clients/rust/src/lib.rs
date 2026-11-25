//! A client for the Gateway inference API, contract 1.0.0.
//!
//! ```no_run
//! use qucelerate_ai_gateway::{ChatRequest, Client, Message};
//!
//! # async fn run() -> Result<(), qucelerate_ai_gateway::Error> {
//! let client = Client::builder("https://gateway.example.com")
//!     .api_key(std::env::var("GATEWAY_API_KEY").unwrap_or_default())
//!     .build()?;
//! let response = client
//!     .chat(&ChatRequest::new("chat-default", vec![Message::user("Say hello.")]))
//!     .await?;
//! println!("{}", response.content().unwrap_or_default());
//! # Ok(())
//! # }
//! ```
//!
//! The client is a transport wrapper: authorization, budgets, routing and
//! settlement stay on the gateway. Every call sends an `Idempotency-Key`
//! (generated unless you pass one) and retries automatically only when the
//! gateway refused the request before reserving anything; see
//! [`ClientBuilder::max_retries`].

mod client;
mod error;
mod sse;
mod types;

pub use client::{ChatStream, Client, ClientBuilder, RequestOptions};
pub use error::{Error, Problem};
pub use types::*;
