//! Command-line tool to archive a list of URLs read from standard input into a WACZ file.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

use cli_helpers::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};
use warc_archiver::client::Archiver;
use warc_archiver::config::{Config, IndexFormat};

fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    let urls = read_urls(std::io::stdin().lock())?;
    let defaults = Config::default();
    let config = Config {
        user_agent: opts.user_agent.unwrap_or(defaults.user_agent),
        timeout: opts.timeout.map_or(defaults.timeout, Duration::from_secs),
        max_redirects: opts.max_redirects.unwrap_or(defaults.max_redirects),
        concurrency: opts.concurrency.unwrap_or(defaults.concurrency),
        gzip_warc: !opts.no_gzip,
        index_format: if opts.compressed_index {
            IndexFormat::zipnum()
        } else {
            IndexFormat::Plain
        },
    };
    let archiver = Archiver::new(config)?;

    // The archiver pulls URLs from the iterator as it dispatches them for download, so the bar
    // tracks dispatches, running at most the configured concurrency ahead of completions. It is
    // cleared before the result is checked so that an error cannot leak a stuck bar.
    let progress = progress_bar(urls.len() as u64, "Archiving", "URLs");
    let result = archiver.archive_to_path(urls.iter().inspect(|_| progress.inc(1)), &opts.output);
    progress.finish_and_clear();
    let summary = result?;

    for failure in &summary.failures {
        log::warn!("Failed to capture {}: {}", failure.url, failure.error);
    }

    println!(
        "Archived {} of {} URLs to {}",
        summary.captures.len(),
        urls.len(),
        opts.output.display()
    );

    Ok(())
}

/// Read one URL per line, trimming surrounding whitespace and skipping blank lines.
fn read_urls<R: BufRead>(reader: R) -> Result<Vec<String>, std::io::Error> {
    let mut urls = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let url = line.trim();

        if !url.is_empty() {
            urls.push(url.to_owned());
        }
    }

    Ok(urls)
}

/// Create a progress bar of `len` steps, labelled with `message` and counting units named `unit`.
fn progress_bar(len: u64, message: &'static str, unit: &str) -> ProgressBar {
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(&format!(
            "{{msg}} [{{bar:40}}] {{human_pos}}/{{human_len}} {unit} ({{eta}})"
        ))
        .expect("valid progress bar template"),
    );
    progress.set_message(message);
    progress
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CLI argument reading error: {0}")]
    Args(#[from] cli_helpers::Error),
    #[error("archiving error: {0}")]
    Archive(#[from] warc_archiver::client::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "warc-archiver", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    /// Path of the WACZ file to write (an existing file is not overwritten).
    #[clap(long)]
    output: PathBuf,
    /// Store the WARC member uncompressed instead of gzip-compressed.
    #[clap(long)]
    no_gzip: bool,
    /// Write the index as a compressed `ZipNum` pair (`index.cdx.gz` and `index.idx`) instead of a
    /// plain-text index.cdx.
    #[clap(long)]
    compressed_index: bool,
    /// The User-Agent header value sent with every request (defaults to the archiver's own).
    #[clap(long)]
    user_agent: Option<String>,
    /// The timeout in seconds for each request (defaults to 30).
    #[clap(long)]
    timeout: Option<u64>,
    /// The maximum number of redirects followed for each URL (defaults to 10).
    #[clap(long)]
    max_redirects: Option<usize>,
    /// The number of URLs downloaded concurrently (defaults to 1).
    #[clap(long)]
    concurrency: Option<usize>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_urls_trims_and_skips_blank_lines() {
        let input = "https://example.com/\n\n  https://example.org/  \n";

        let urls = super::read_urls(input.as_bytes()).expect("read URLs");

        assert_eq!(urls, ["https://example.com/", "https://example.org/"]);
    }
}
