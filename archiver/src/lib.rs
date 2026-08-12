//! Archiving web pages over HTTP into WACZ web archive collections.
//!
//! This crate provides a small archiving client which downloads a list of URLs and packages the
//! results as a [WACZ](https://specs.webrecorder.net/wacz/1.1.1/) file: a WARC file recording the
//! full HTTP request and response for every exchange (including each hop of a redirect chain), a
//! CDXJ index over the responses, and a page list entry for every archived URL.
//!
//! # Examples
//!
//! ```no_run
//! use warc_archiver::client::Archiver;
//! use warc_archiver::config::Config;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let archiver = Archiver::new(Config::default())?;
//! let summary = archiver.archive_to_path(["https://www.example.com/"], "example.wacz")?;
//!
//! assert!(summary.is_complete());
//! # Ok(())
//! # }
//! ```
//!
//! # Modules
//!
//! * [`client`]: the archiving client and its outcome types
//! * [`config`]: client configuration
#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    rust_2018_idioms
)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
mod http;
