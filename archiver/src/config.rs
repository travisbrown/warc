//! Configuration for the archiving client.

use std::time::Duration;

/// The default `User-Agent` header value, identifying this crate and its version.
pub const DEFAULT_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Configuration for the archiving client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// The `User-Agent` header value sent with every request.
    pub user_agent: String,
    /// The timeout for each request, from connecting until the response body has been read.
    pub timeout: Duration,
    /// The maximum number of redirects followed for each URL.
    ///
    /// Every hop is captured; when a response still redirects after this many follows, it is
    /// recorded as the final response for its URL rather than treated as an error.
    pub max_redirects: usize,
}

impl Default for Config {
    /// The default configuration: this crate's `User-Agent`, a 30-second timeout, and at most
    /// ten redirects per URL.
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            timeout: Duration::from_secs(30),
            max_redirects: 10,
        }
    }
}
