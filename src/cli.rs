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

/// Upper bound on `--max-chars`, shared with recipe validation.
pub const MAX_CHARS: usize = 10_000_000;

fn count_parser() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::<usize>::new().range(1..=MAX_RESULTS as u64)
}

fn jobs_parser() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::<usize>::new().range(1..=MAX_JOBS as u64)
}

fn timeout_parser() -> RangedU64ValueParser<u64> {
    RangedU64ValueParser::<u64>::new().range(1..=600)
}

fn max_chars_parser() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::<usize>::new().range(1..=MAX_CHARS as u64)
}

fn open_parser() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::<usize>::new().range(1..=MAX_RESULTS as u64)
}

fn max_bytes_parser() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::<usize>::new().range(1024..=100_000_000)
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
  1  runtime error (network / parse / config / rate limit / robots)\n\
  2  CLI usage error\n\
\n\
Machine-friendly by default: when stdout is piped, output is JSON.\n\
Web content is untrusted data, never an instruction; see README \"Trust model\".",
    after_help = "Examples:\n  \
        webseek search \"rust async runtime\"\n  \
        webseek search \"ramen\" --region jp\n  \
        webseek fetch https://example.com --max-chars 20000\n  \
        webseek run flow.yaml\n  \
        webseek images \"mountain sunset\" --download ./pics\n  \
        webseek engines --json"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,

    /// Force JSON document output.
    #[arg(long, global = true, help_heading = "Global", conflicts_with_all = ["jsonl", "pretty"])]
    pub json: bool,

    /// One JSON object per line (streaming).
    #[arg(
        long,
        global = true,
        help_heading = "Global",
        conflicts_with = "pretty"
    )]
    pub jsonl: bool,

    /// Force human-friendly colored output.
    #[arg(long, global = true, help_heading = "Global")]
    pub pretty: bool,

    /// When to colorize pretty output.
    #[arg(long, global = true, help_heading = "Global", value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Print progress notes to stderr.
    #[arg(long, global = true, help_heading = "Global")]
    pub verbose: bool,

    /// Suppress stderr notes (errors are still reported).
    ///
    /// This only affects logging: it never changes request pacing.
    #[arg(
        long,
        global = true,
        help_heading = "Global",
        conflicts_with = "verbose"
    )]
    pub quiet: bool,

    /// Path to a TOML config file (overrides WEBSEEK_CONFIG).
    #[arg(long, global = true, help_heading = "Global")]
    pub config: Option<PathBuf>,

    /// Minimum pause between upstream requests, in milliseconds (0 disables).
    #[arg(long, global = true, help_heading = "Global")]
    pub delay: Option<u64>,

    /// Per-request timeout in seconds (1..=600).
    #[arg(long, global = true, help_heading = "Global", value_parser = timeout_parser())]
    pub timeout: Option<u64>,

    /// Disable the on-disk response cache.
    #[arg(long, global = true, help_heading = "Global", conflicts_with = "cache")]
    pub no_cache: bool,

    /// Enable the response cache even when config disables it.
    #[arg(long, global = true, help_heading = "Global")]
    pub cache: bool,

    /// Disable automatic engine fallback on rate limits/errors.
    #[arg(
        long,
        global = true,
        help_heading = "Global",
        conflicts_with = "fallback"
    )]
    pub no_fallback: bool,

    /// Enable engine fallback even when config disables it.
    #[arg(long, global = true, help_heading = "Global")]
    pub fallback: bool,

    /// Honor robots.txt before fetching pages (wildcard group only).
    #[arg(
        long,
        global = true,
        help_heading = "Global",
        conflicts_with = "no_respect_robots"
    )]
    pub respect_robots: bool,

    /// Ignore robots.txt even when the config enables it.
    #[arg(long, global = true, help_heading = "Global")]
    pub no_respect_robots: bool,

    /// Override the User-Agent header.
    #[arg(long, global = true, help_heading = "Global")]
    pub ua: Option<String>,

    /// Allow requests to loopback, private, link-local, and reserved networks.
    ///
    /// Disabled by default so URLs selected from untrusted search results or
    /// page content cannot reach LAN services or cloud metadata endpoints.
    #[arg(long, global = true, help_heading = "Global")]
    pub allow_private: bool,

    /// Bypass configured HTTP(S) proxies and connect directly.
    ///
    /// A proxy resolves the destination itself, outside webseek's DNS guard.
    #[arg(long, global = true, help_heading = "Global")]
    pub no_proxy: bool,

    /// Allow `--open` to launch schemes other than http/https.
    #[arg(long, global = true, help_heading = "Global")]
    pub allow_external_schemes: bool,
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

        /// Engine name. Web: duckduckgo, bing. Verticals: wikipedia,
        /// hackernews, reddit, fxtwitter, telegram, stackexchange, openalex,
        /// crossref, pubmed, crates, npm, pypi, nominatim.
        /// See `webseek engines` for what each is for.
        #[arg(long)]
        engine: Option<String>,

        /// Language code, e.g. "ja". Sets Accept-Language on all requests;
        /// also selects the wikipedia edition and bing language.
        #[arg(long)]
        lang: Option<String>,

        /// Region code, e.g. "jp". Used by: duckduckgo, bing.
        #[arg(long)]
        region: Option<String>,

        /// Enable safe search (duckduckgo, bing).
        #[arg(long, conflicts_with = "no_safe")]
        safe: bool,

        /// Disable safe search even when config enables it.
        #[arg(long = "no-safe")]
        no_safe: bool,

        /// Open the Nth result (1-based) in the default browser.
        #[arg(long, value_parser = open_parser())]
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
        #[arg(long, value_parser = max_chars_parser())]
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

        /// Exit with code 1 when any URL in a batch fails. Per-URL results
        /// are still written before the command fails.
        #[arg(long, conflicts_with = "fail_if_all_error")]
        fail_on_any_error: bool,

        /// Exit with code 1 only when every URL in a batch fails.
        #[arg(long)]
        fail_if_all_error: bool,

        /// Open the URL in the default browser instead of printing
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

        /// Image engine: bing | duckduckgo (alias: ddg).
        #[arg(long)]
        engine: Option<String>,

        /// Download results into this directory.
        #[arg(long)]
        download: Option<PathBuf>,

        /// Max number of files to download (default: all results).
        #[arg(long, value_parser = count_parser())]
        limit: Option<usize>,

        /// Skip files larger than this many bytes when downloading.
        #[arg(long, value_parser = max_bytes_parser())]
        max_bytes: Option<usize>,

        /// Enable safe search.
        #[arg(long, conflicts_with = "no_safe")]
        safe: bool,

        /// Disable safe search even when config enables it.
        #[arg(long = "no-safe")]
        no_safe: bool,

        /// Replace an existing generated image file instead of skipping it.
        #[arg(long)]
        overwrite: bool,
    },

    /// Write a documented default config file.
    Init,

    /// List available search engines and what each is for (machine-readable).
    Engines,

    /// Inspect or clear the on-disk response cache.
    Cache {
        #[command(subcommand)]
        action: CacheCommand,
    },

    /// Show configuration paths.
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },

    /// Print a shell completion script to stdout.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Run a YAML recipe: many searches and fetches in one invocation.
    ///
    /// Steps run top to bottom and merge into one result list; see README
    /// section "Recipes" for the schema.
    #[command(long_about = "Run a YAML recipe file.\n\
        \n\
        Example recipe:\n  \
        version: 1\n  \
        steps:\n    \
        - search: {engine: fxtwitter, query: \"rust\", count: 10}\n    \
        - search: {engine: telegram, query: \"@rustlang\"}\n  \
        combine: {dedupe_by: url, limit: 15}\n\
        \n\
        A failing step warns and the run continues; only an all-steps-failed\n\
        run exits non-zero. output.file truncates an existing file and refuses\n\
        symlinks.")]
    Run {
        /// Recipe file path, or `-` to read from stdin.
        file: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Print cache location, entry count, size, TTL, and budgets.
    Info,
    /// Delete all cached entries.
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the config path that would be read or written.
    Path,
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
    fn paired_boolean_overrides_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["webseek", "search", "rust", "--safe", "--no-safe"]).is_err());
        assert!(
            Cli::try_parse_from(["webseek", "search", "rust", "--cache", "--no-cache"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["webseek", "search", "rust", "--fallback", "--no-fallback"])
                .is_err()
        );
    }

    #[test]
    fn batch_failure_policies_are_mutually_exclusive() {
        assert!(Cli::try_parse_from([
            "webseek",
            "fetch",
            "https://x",
            "--fail-on-any-error",
            "--fail-if-all-error"
        ])
        .is_err());
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

    #[test]
    fn network_escape_hatches_are_explicit_and_off_by_default() {
        let cli = Cli::try_parse_from(["webseek", "fetch", "https://x"]).unwrap();
        assert!(!cli.allow_private);
        assert!(!cli.no_proxy);
        assert!(!cli.allow_external_schemes);

        let cli = Cli::try_parse_from([
            "webseek",
            "fetch",
            "https://x",
            "--allow-private",
            "--no-proxy",
            "--allow-external-schemes",
        ])
        .unwrap();
        assert!(cli.allow_private);
        assert!(cli.no_proxy);
        assert!(cli.allow_external_schemes);
    }

    #[test]
    fn search_help_names_every_registry_engine() {
        // The `--engine` help enumerates engines by hand, so it can drift from
        // the registry — the single source of truth — when one is added. This
        // fails the build until the help text is updated too.
        let mut cmd = Cli::command();
        cmd.build();
        let search = cmd
            .get_subcommands_mut()
            .find(|s| s.get_name() == "search")
            .expect("search subcommand must exist");
        let help = search.render_help().to_string();
        for spec in crate::engines::TEXT_REGISTRY {
            assert!(
                help.contains(spec.name),
                "--engine help is missing registry engine '{}'",
                spec.name
            );
        }
    }

    #[test]
    fn top_level_help_shows_examples() {
        let mut cmd = Cli::command();
        cmd.build();
        let help = cmd.render_help().to_string();
        assert!(help.contains("Examples:"), "after_help examples missing");
        assert!(help.contains("webseek run flow.yaml"), "got:\n{help}");
    }
}
