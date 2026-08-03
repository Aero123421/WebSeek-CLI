//! Command-line interface definition (clap derive).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "webseek",
    version,
    about = "API-key-free web search & page fetch for AI agents",
    long_about = "webseek — API-key-free web search and page reader designed for AI agents.\n\
\n\
Exit codes:\n\
  0  success (including \"no results\": empty JSON is valid)\n\
  1  runtime error (network / parse / config)\n\
  2  CLI usage error\n\
\n\
Machine-friendly by default: when stdout is piped, output is JSON."
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,

    /// Force JSON document output.
    #[arg(long, global = true)]
    pub json: bool,

    /// One JSON object per line (streaming).
    #[arg(long, global = true)]
    pub jsonl: bool,

    /// Force human-friendly colored output.
    #[arg(long, global = true)]
    pub pretty: bool,

    /// Print progress notes to stderr.
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Suppress all stderr notes (except errors).
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Path to a TOML config file (default: platform config dir).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Pause between upstream requests, in milliseconds (0 disables).
    #[arg(long, global = true)]
    pub delay: Option<u64>,

    /// Per-request timeout in seconds.
    #[arg(long, global = true)]
    pub timeout: Option<u64>,

    /// Disable the on-disk response cache.
    #[arg(long, global = true)]
    pub no_cache: bool,

    /// Disable automatic engine fallback on rate limits/errors.
    #[arg(long, global = true)]
    pub no_fallback: bool,

    /// Honor robots.txt before fetching pages (wildcard group only).
    #[arg(long, global = true)]
    pub respect_robots: bool,

    /// Override the User-Agent header.
    #[arg(long, global = true)]
    pub ua: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Search the web and print results.
    Search {
        /// Search query.
        query: String,

        /// Number of results (1..=50).
        #[arg(short, long, default_value_t = 5)]
        count: usize,

        /// Engine: duckduckgo | bing.
        #[arg(long)]
        engine: Option<String>,

        /// Language code, e.g. "ja" (engine-dependent).
        #[arg(long)]
        lang: Option<String>,

        /// Region code, e.g. "jp-jp" (engine-dependent).
        #[arg(long)]
        region: Option<String>,

        /// Enable safe search.
        #[arg(long)]
        safe: bool,

        /// Open the Nth result (1-based) in the default browser.
        #[arg(long)]
        open: Option<usize>,
    },

    /// Fetch one or more pages and print main content as clean text.
    ///
    /// With multiple URLs, pages are fetched in parallel (see `--jobs`)
    /// and printed as one JSON array preserving input order.
    Fetch {
        /// Page URLs (1 or more).
        #[arg(required = true)]
        urls: Vec<String>,

        /// Character cap on extracted text.
        #[arg(long, default_value_t = 20_000)]
        max_chars: usize,

        /// Keep light markdown (headings, lists, links).
        #[arg(long)]
        markdown: bool,

        /// Dump raw HTML instead of extracted text.
        #[arg(long)]
        html: bool,

        /// Parallel workers for multi-URL fetches (default: 1).
        #[arg(short = 'j', long, default_value_t = 1)]
        jobs: usize,

        /// Open the page in the default browser instead of printing
        /// (requires exactly one URL).
        #[arg(long)]
        open: bool,
    },

    /// Search for images (optionally download them).
    Images {
        /// Search query.
        query: String,

        /// Number of results (1..=50).
        #[arg(short, long, default_value_t = 5)]
        count: usize,

        /// Image engine: bing | duckduckgo.
        #[arg(long)]
        engine: Option<String>,

        /// Download results into this directory.
        #[arg(long)]
        download: Option<PathBuf>,

        /// Max number of files to download (default: all results).
        #[arg(long)]
        limit: Option<usize>,

        /// Skip files larger than this many bytes when downloading.
        #[arg(long)]
        max_bytes: Option<usize>,

        /// Enable safe search.
        #[arg(long)]
        safe: bool,
    },

    /// Write a documented default config file.
    Init,

    /// List available search engines and what each is for (machine-readable).
    Engines,
}
