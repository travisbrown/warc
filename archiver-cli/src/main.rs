//! Command-line tool to archive a list of URLs read from standard input into a WACZ file.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

use std::io::BufRead;
use std::path::PathBuf;

use cli_helpers::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};
use warc_archiver::client::Archiver;
use warc_archiver::config::{Config, IndexFormat};

fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    let urls = read_urls(std::io::stdin().lock())?;
    let config = Config {
        user_agent: USER_AGENT.into(),
        gzip_warc: !opts.no_gzip,
        index_format: if opts.compressed_index {
            IndexFormat::zipnum()
        } else {
            IndexFormat::Plain
        },
        ..Default::default()
    };
    let archiver = Archiver::new(config)?;

    // The archiver pulls URLs from the iterator one at a time as it downloads them, so advancing
    // the bar as each URL is drawn tracks the downloads themselves.
    let progress = progress_bar(urls.len() as u64, "Archiving", "URLs");
    let summary =
        archiver.archive_to_path(urls.iter().inspect(|_| progress.inc(1)), &opts.output)?;
    progress.finish_and_clear();

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
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("CLI argument reading error")]
    Args(#[from] cli_helpers::Error),
    #[error("archiving error")]
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
    /// Write the index as a compressed ZipNum pair (index.cdx.gz and index.idx) instead of a
    /// plain-text index.cdx.
    #[clap(long)]
    compressed_index: bool,
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
