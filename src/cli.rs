//! Command-line interface definition (clap derive).
//!
//! Options that also exist in `config.toml` are `Option<T>` on purpose: with a
//! `default_value_t` there is no way to tell "the user asked for 5" from "the
//! user said nothing", and the config value can never win. `None` means
//! "unspecified — take it from the config".

use std::path::PathBuf;

use clap::builder::RangedU64ValueParser;
use clap::{Parser, Subcommand, ValueEnum};

/// Upper bound on `--count`, shared with the config loader.
pub const MAX_RESULTS: usize = 50;

/// Upper bound on `--jobs`. Beyond this the thread pool costs more than the
/// concurrency buys, and upstreams stop enjoying our company.
pub const MAX_JOBS: usize = 64;

fn count_parser() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::<usize>::new().range(1..=MAX_RESULTS as u64)
}

fn jobs_parser() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::<usize>::new().range(1..=MAX_JOBS as u64)
}

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
    #[arg(long, global = true, conflicts_with_all = ["jsonl", "pretty"])]
    pub json: bool,

    /// One JSON object per line (streaming).
    #[arg(long, global = true, conflicts_with = "pretty")]
    pub jsonl: bool,

    /// Force human-friendly colored output.
    #[arg(long, global = true)]
    pub pretty: bool,

    /// When to colorize pretty output.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Print progress notes to stderr.
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Suppress stderr notes (errors are still reported).
    ///
    /// This only affects logging: it never changes request pacing.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Path to a TOML config file (overrides WEBSEEK_CONFIG).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Minimum pause between upstream requests, in milliseconds (0 disables).
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
    #[arg(long, global = true, conflicts_with = "no_respect_robots")]
    pub respect_robots: bool,

    /// Ignore robots.txt even when the config enables it.
    #[arg(long, global = true)]
    pub no_respect_robots: bool,

    /// Override the User-Agent header.
    #[arg(long, global = true)]
    pub ua: Option<String>,
}

impl Cli {
    /// Effective robots.txt setting: an explicit CLI flag wins over config.
    pub fn respect_robots(&self, cfg_value: bool) -> bool {
        if self.no_respect_robots {
            false
        } else {
            cfg_value || self.respect_robots
        }
    }
}

/// `--color` policy, mirroring the de-facto standard (`NO_COLOR`, TTY check).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Colorize only when stdout is a terminal and NO_COLOR is unset.
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Search the web and print results.
    Search {
        /// Search query.
        query: String,

        /// Number of results (1..=50). Default: config `max_results`, else 5.
        #[arg(short, long, value_parser = count_parser())]
        count: Option<usize>,

        /// Engine name; see `webseek engines`.
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

        /// Character cap on output. Default: config `max_chars`, else 20000.
        #[arg(long)]
        max_chars: Option<usize>,

        /// Keep light markdown (headings, lists, links).
        #[arg(long, conflicts_with = "html")]
        markdown: bool,

        /// Dump raw HTML instead of extracted text (still capped by --max-chars).
        #[arg(long)]
        html: bool,

        /// Always emit a JSON array, even for a single URL.
        ///
        /// Lets an agent parse one shape regardless of how many URLs it passed.
        #[arg(long)]
        array: bool,

        /// Parallel workers for multi-URL fetches (1..=64, default 1).
        #[arg(short = 'j', long, default_value_t = 1, value_parser = jobs_parser())]
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

        /// Number of results (1..=50). Default: config `max_results`, else 5.
        #[arg(short, long, value_parser = count_parser())]
        count: Option<usize>,

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

    /// Print a shell completion script to stdout.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn count_and_max_chars_default_to_unspecified() {
        let cli = Cli::try_parse_from(["webseek", "search", "rust"]).unwrap();
        match cli.cmd {
            Command::Search { count, .. } => assert_eq!(
                count, None,
                "an unspecified --count must stay None so config can win"
            ),
            _ => panic!("wrong subcommand"),
        }
        let cli = Cli::try_parse_from(["webseek", "fetch", "https://x"]).unwrap();
        match cli.cmd {
            Command::Fetch { max_chars, .. } => assert_eq!(max_chars, None),
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn out_of_range_count_is_a_usage_error_not_a_silent_clamp() {
        let err = Cli::try_parse_from(["webseek", "search", "rust", "--count", "100"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(Cli::try_parse_from(["webseek", "search", "rust", "--count", "50"]).is_ok());
        assert!(Cli::try_parse_from(["webseek", "search", "rust", "--count", "0"]).is_err());
    }

    #[test]
    fn jobs_is_bounded() {
        assert!(Cli::try_parse_from(["webseek", "fetch", "https://x", "-j", "64"]).is_ok());
        assert!(Cli::try_parse_from(["webseek", "fetch", "https://x", "-j", "1000"]).is_err());
        assert!(Cli::try_parse_from(["webseek", "fetch", "https://x", "-j", "0"]).is_err());
    }

    #[test]
    fn conflicting_output_modes_are_rejected() {
        assert!(Cli::try_parse_from(["webseek", "engines", "--json", "--pretty"]).is_err());
        assert!(Cli::try_parse_from(["webseek", "engines", "--json", "--jsonl"]).is_err());
        assert!(Cli::try_parse_from(["webseek", "engines", "--jsonl", "--pretty"]).is_err());
    }

    #[test]
    fn robots_can_be_turned_off_from_the_command_line() {
        let cli = Cli::try_parse_from(["webseek", "fetch", "https://x", "--no-respect-robots"])
            .expect("--no-respect-robots must exist");
        assert!(
            !cli.respect_robots(true),
            "config respect_robots=true must be overridable"
        );

        let cli = Cli::try_parse_from(["webseek", "fetch", "https://x"]).unwrap();
        assert!(cli.respect_robots(true), "config value applies by default");
        assert!(!cli.respect_robots(false));

        let cli =
            Cli::try_parse_from(["webseek", "fetch", "https://x", "--respect-robots"]).unwrap();
        assert!(cli.respect_robots(false), "flag enables it on its own");

        // The two flags are mutually exclusive rather than silently ordered.
        assert!(Cli::try_parse_from([
            "webseek",
            "fetch",
            "https://x",
            "--respect-robots",
            "--no-respect-robots"
        ])
        .is_err());
    }
}
