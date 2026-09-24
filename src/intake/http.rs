//! Shared HTTP bits for the tracker pollers (rustls, no default features).

use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("{0}")]
    Transport(#[from] reqwest::Error),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("{0}")]
    Api(String),
}

pub fn client() -> Result<reqwest::Client, HttpError> {
    Ok(reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("dispatch/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
