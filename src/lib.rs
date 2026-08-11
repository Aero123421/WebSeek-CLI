//! Library root — all logic lives here so it can be tested;
//! `main.rs` is a thin wrapper.

pub mod batch;
pub mod cache;
pub mod cli;
pub mod config;
pub mod engines;
pub mod error;
pub mod http;
pub mod models;
pub mod output;
pub mod pace;
pub mod reader;
pub mod region;
pub mod robots;
pub mod text;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::batch::{BatchCtx, BatchItem};
use crate::cache::Cache;
use crate::cli::{Cli, Command};
use crate::engines::{engine_by_name, image_engine_by_name};
use crate::error::Error;
use crate::models::{FetchOpts, SearchOpts, SearchResult};
use crate::output::{write_fetch, write_fetch_batch, write_images, write_search, Mode};
use crate::pace::Pacer;
use crate::robots::RobotsChecker;

/// A cached search answer.
///
/// The engine that actually answered is stored alongside the results, so a
/// cache hit reports the same `engine` the live call did instead of echoing
/// back whatever the caller asked for.
#[derive(Serialize, Deserialize)]
struct CachedSearch<T> {
    engine: String,
    results: Vec<T>,
}

/// Entry point used by `main.rs`.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    output::set_quiet(cli.quiet);
    output::set_color(cli.color);
    let mode = output_mode(&cli);

    // These three commands must work before (and without) any config.
    match &cli.cmd {
        Command::Init => {
            let path = config::Config::write_default(cli.config.as_deref())?;
            output::note(&format!("wrote config to {}", path.display()));
            return Ok(());
        }
        Command::Engines => {
            output::write_engines(mode, &engines::catalog())?;
            return Ok(());
        }
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            let bin = cmd.get_name().to_string();
            clap_complete::generate(*shell, &mut cmd, bin, &mut std::io::stdout());
            return Ok(());
        }
        _ => {}
    }

    let cfg = config::Config::load(cli.config.as_deref())?;
    config::validate_engine(&cfg.engine)?;
    config::validate_image_engine(&cfg.image_engine)?;

    let delay = cli.delay.map(Duration::from_millis).unwrap_or(cfg.delay);
    let timeout = cli.timeout.map(Duration::from_secs).unwrap_or(cfg.timeout);
    let user_agent = cli.ua.as_deref().unwrap_or(&cfg.user_agent);
    let client = http::build_client(timeout, user_agent)?;

    let cache = Arc::new(Mutex::new(if cli.no_cache || cfg.cache_max_entries == 0 {
        Cache::disabled()
    } else {
        Cache::load(
            cache::default_cache_path(),
            cfg.cache_ttl_secs,
            cfg.cache_max_entries,
        )
    }));
    let robots = Arc::new(Mutex::new(RobotsChecker::new()));
    let pacer = Arc::new(Pacer::new(delay));

    let ctx = Ctx {
        cli: &cli,
        cfg: &cfg,
        client: &client,
        mode,
        cache: &cache,
        robots: &robots,
        pacer: &pacer,
    };
    let outcome = run_inner(&ctx);
    if let Ok(c) = cache.lock() {
        c.save(); // best-effort, even when the command failed
    }
    // Flush explicitly so a write failure is reported here rather than being
    // swallowed when stdout is dropped at exit.
    let _ = std::io::stdout().flush();
    outcome
}

/// Everything the subcommands share.
struct Ctx<'a> {
    cli: &'a Cli,
    cfg: &'a config::Config,
    client: &'a Client,
    mode: Mode,
    cache: &'a Arc<Mutex<Cache>>,
    robots: &'a Arc<Mutex<RobotsChecker>>,
    pacer: &'a Arc<Pacer>,
}

impl Ctx<'_> {
    /// Emit a progress note when `--verbose` is on.
    fn trace(&self, msg: &str) {
        if self.cli.verbose {
            output::note(msg);
        }
    }

    fn search_opts(
        &self,
        count: Option<usize>,
        lang: &Option<String>,
        region: &Option<String>,
        safe: bool,
    ) -> SearchOpts {
        SearchOpts {
            // An explicit flag wins; otherwise the config value applies.
            count: count
                .unwrap_or(self.cfg.max_results)
                .clamp(1, cli::MAX_RESULTS),
            lang: lang.clone().or_else(|| self.cfg.lang.clone()),
            region: region.clone().or_else(|| self.cfg.region.clone()),
            safe: safe || self.cfg.safe_search,
            api_user_agent: config::api_user_agent_with_contact(self.cfg.contact_email.as_deref()),
            contact_email: self.cfg.contact_email.clone(),
        }
    }
}

fn run_inner(ctx: &Ctx<'_>) -> Result<()> {
    let respect_robots = ctx.cli.respect_robots(ctx.cfg.respect_robots);

    match &ctx.cli.cmd {
        Command::Init | Command::Engines | Command::Completions { .. } => {
            unreachable!("handled before config load")
        }

        Command::Search {
            query,
            count,
            engine,
            lang,
            region,
            safe,
            open,
        } => {
            let name = engine.as_deref().unwrap_or(&ctx.cfg.engine);
            let opts = ctx.search_opts(*count, lang, region, *safe);
            ctx.trace(&format!("searching '{query}' via {name}"));

            let (results, engine_used) = search_with_cache(ctx, name, query, &opts)?;
            // Print the results first: `--open` is an extra action, not a
            // reason to produce no output on a `--json` run.
            write_search(ctx.mode, query, engine_used, &results)?;

            if let Some(idx) = open {
                let i = idx
                    .checked_sub(1)
                    .ok_or_else(|| Error::Config("--open index must be >= 1".to_string()))?;
                let target = results.get(i).ok_or_else(|| {
                    Error::NoResults(format!("no result #{idx} (only {} found)", results.len()))
                })?;
                open::that(&target.url)
                    .map_err(|e| Error::Network(format!("cannot open browser: {e}")))?;
            }
            Ok(())
        }

        Command::Fetch {
            urls,
            max_chars,
            markdown,
            html,
            array,
            jobs,
            open,
        } => {
            if *open {
                if urls.len() != 1 {
                    return Err(Error::Config("--open requires exactly one URL".to_string()).into());
                }
                open::that(&urls[0])
                    .map_err(|e| Error::Network(format!("cannot open browser: {e}")))?;
                return Ok(());
            }

            let opts = FetchOpts {
                max_bytes: reader::DEFAULT_MAX_BYTES,
                max_chars: max_chars.unwrap_or(ctx.cfg.max_chars).max(1),
                raw_html: *html,
                markdown: *markdown,
            };
            let batch_ctx = BatchCtx {
                client: ctx.client,
                opts: &opts,
                cache: ctx.cache,
                robots: ctx.robots,
                pacer: ctx.pacer,
                respect_robots,
            };

            // One URL without `--array` keeps the single-object contract, and
            // a failure is then a command failure. In array mode failures are
            // data, so the command still succeeds.
            if urls.len() == 1 && !*array {
                let (fetched, cached) = batch::fetch_one(&batch_ctx, &urls[0])?;
                ctx.trace(&if cached {
                    format!("{} (cache hit)", urls[0])
                } else {
                    format!("fetched {}", urls[0])
                });
                write_fetch(ctx.mode, &fetched)?;
                return Ok(());
            }

            ctx.trace(&format!(
                "fetching {} URL(s) with {} worker(s)",
                urls.len(),
                (*jobs).max(1)
            ));
            let items = batch::fetch_many(&batch_ctx, urls, *jobs);
            write_fetch_batch(ctx.mode, &items)?;
            summarize_batch(ctx, &items);
            Ok(())
        }

        Command::Images {
            query,
            count,
            engine,
            download,
            limit,
            max_bytes,
            safe,
        } => {
            let name = engine.as_deref().unwrap_or(&ctx.cfg.image_engine);
            let count = count
                .unwrap_or(ctx.cfg.max_results)
                .clamp(1, cli::MAX_RESULTS);
            let safe = *safe || ctx.cfg.safe_search;
            ctx.trace(&format!("searching images for '{query}' via {name}"));

            let (results, engine_used) = images_with_cache(ctx, name, query, count, safe)?;
            let downloaded = match download {
                Some(dir) => {
                    let dl_ctx = engines::images::DownloadCtx {
                        client: ctx.client,
                        pacer: ctx.pacer,
                        robots: respect_robots.then_some(ctx.robots.as_ref()),
                        limit: limit.unwrap_or(results.len()),
                        max_bytes: max_bytes.unwrap_or(ctx.cfg.image_max_bytes),
                    };
                    Some(engines::images::download(&dl_ctx, &results, dir)?)
                }
                None => None,
            };
            write_images(ctx.mode, query, engine_used, &results, downloaded)?;
            Ok(())
        }
    }
}

/// Tell the user on stderr how a partially-failed batch went; stdout stays
/// pure data, and the exit code stays 0 because the errors *are* the answer.
fn summarize_batch(ctx: &Ctx<'_>, items: &[BatchItem]) {
    let failed = items.iter().filter(|i| i.is_err()).count();
    if failed > 0 {
        ctx.trace(&format!("{failed}/{} URL(s) failed", items.len()));
    }
}

/// Run `attempt` over the fallback chain, returning the first useful answer.
///
/// "Useful" excludes an empty result set: a scraper whose selectors stopped
/// matching returns `Ok(vec![])`, which is by far the most common way these
/// engines break. Treating that as success meant the headline fallback feature
/// never fired for the failure it most needed to cover. If *every* engine
/// agrees there is nothing, that is a real zero-result answer (exit 0).
fn try_engines<T>(
    ctx: &Ctx<'_>,
    order: &[&'static str],
    mut attempt: impl FnMut(&'static str) -> error::Result<(Vec<T>, &'static str)>,
) -> Result<(Vec<T>, &'static str)> {
    let mut last_err: Option<Error> = None;
    let mut empty_from: Option<&'static str> = None;

    for name in order {
        match attempt(name) {
            Ok((results, used)) if !results.is_empty() => return Ok((results, used)),
            Ok((_, used)) => {
                empty_from.get_or_insert(used);
                if order.len() > 1 {
                    ctx.trace(&format!(
                        "{used} returned no results, trying the next engine"
                    ));
                }
            }
            Err(e) => {
                if !e.is_engine_retryable() {
                    return Err(e.into());
                }
                ctx.trace(&format!("{name} failed ({e})"));
                last_err = Some(e);
            }
        }
    }

    match (empty_from, last_err) {
        // Every engine answered "nothing found" — that is a valid answer.
        (Some(used), _) => Ok((Vec::new(), used)),
        (None, Some(e)) => Err(e.into()),
        (None, None) => Err(Error::NoResults("no engine was available".into()).into()),
    }
}

/// Search honoring the cache, with automatic engine fallback.
fn search_with_cache(
    ctx: &Ctx<'_>,
    name: &str,
    query: &str,
    opts: &SearchOpts,
) -> Result<(Vec<SearchResult>, &'static str)> {
    // Validate up front: an unknown engine is the user's mistake and must
    // surface as-is rather than being quietly rerouted.
    engine_by_name(name)?;

    let key = cache::search_key(name, query, opts);
    if let Some(hit) = cached::<SearchResult>(ctx, &key) {
        ctx.trace(&format!("{} results (cache hit)", hit.1));
        return Ok(hit);
    }

    let order = if ctx.cfg.fallback && !ctx.cli.no_fallback {
        engines::fallback_order(name)
    } else {
        vec![engines::canonical_engine(name).unwrap_or("duckduckgo")]
    };

    let (results, engine_used) = try_engines(ctx, &order, |eng_name| {
        let eng = engine_by_name(eng_name)?;
        eng.search(ctx.client, query, opts).map(|r| (r, eng.name()))
    })?;

    store(ctx, key, engine_used, &results);
    Ok((results, engine_used))
}

/// Image search honoring the cache, with automatic engine fallback.
fn images_with_cache(
    ctx: &Ctx<'_>,
    name: &str,
    query: &str,
    count: usize,
    safe: bool,
) -> Result<(Vec<models::ImageResult>, &'static str)> {
    image_engine_by_name(name)?;

    let key = cache::images_key(name, query, count, safe);
    if let Some(hit) = cached::<models::ImageResult>(ctx, &key) {
        ctx.trace(&format!("{} image results (cache hit)", hit.1));
        return Ok(hit);
    }

    let order = if ctx.cfg.fallback && !ctx.cli.no_fallback {
        engines::image_fallback_order(name)
    } else {
        vec![engines::canonical_image_engine(name).unwrap_or("bing")]
    };

    let (results, engine_used) = try_engines(ctx, &order, |eng_name| {
        let eng = image_engine_by_name(eng_name)?;
        eng.search(ctx.client, query, count, safe)
            .map(|r| (r, eng.name()))
    })?;

    store(ctx, key, engine_used, &results);
    Ok((results, engine_used))
}

/// Read a cached answer, resolving the stored engine name back to a static str.
fn cached<T: for<'de> Deserialize<'de>>(
    ctx: &Ctx<'_>,
    key: &str,
) -> Option<(Vec<T>, &'static str)> {
    let value = ctx.cache.lock().ok().and_then(|c| c.get(key))?;
    let hit: CachedSearch<T> = serde_json::from_value(value).ok()?;
    let engine = engines::canonical_engine(&hit.engine)
        .or_else(|| engines::canonical_image_engine(&hit.engine))?;
    Some((hit.results, engine))
}

/// Store an answer under the key the *request* produced.
fn store<T: Serialize>(ctx: &Ctx<'_>, key: String, engine_used: &str, results: &[T]) {
    let Ok(mut c) = ctx.cache.lock() else { return };
    let doc = serde_json::to_value(CachedSearch {
        engine: engine_used.to_string(),
        results: results.iter().collect::<Vec<_>>(),
    });
    if let Ok(v) = doc {
        c.put(key, v);
    }
}

fn output_mode(cli: &Cli) -> Mode {
    if cli.json {
        Mode::Json
    } else if cli.jsonl {
        Mode::Jsonl
    } else if cli.pretty {
        Mode::Pretty
    } else {
        Mode::auto()
    }
}
