//! Library root — all logic lives here so it can be tested;
//! `main.rs` is a thin wrapper.

pub mod batch;
pub mod cache;
pub mod cli;
pub mod config;
pub mod engines;
pub mod error;
pub mod feed;
pub mod http;
pub mod models;
pub mod net;
pub mod output;
pub mod ratelimit;
pub mod reader;
pub mod region;
pub mod robots;
pub mod text;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::cache::Cache;
use crate::cli::{CacheCommand, Cli, Command, ConfigCommand};
use crate::error::Error;
use crate::http::Http;
use crate::models::{FetchOpts, FetchResult, SearchOpts};
use crate::output::{
    write_fetch, write_fetch_batch, write_images, write_search, EngineOutcome, Mode,
};
use crate::ratelimit::RateLimiter;
use crate::robots::RobotsChecker;

/// Entry point used by `main.rs`. Returns a real process exit code rather
/// than relying on `anyhow`'s default `Termination` impl, so usage errors
/// (2), runtime errors (1), and a downstream reader closing the pipe early
/// (0 — the standard Unix convention for `SIGPIPE`-like conditions) are all
/// distinguished.
pub fn run() -> std::process::ExitCode {
    let cli = Cli::parse();
    let mode = output_mode(&cli);
    match run_dispatch(&cli, mode) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => finish_with_error(&err, mode, cli.quiet),
    }
}

fn finish_with_error(err: &anyhow::Error, mode: Mode, quiet: bool) -> std::process::ExitCode {
    if is_broken_pipe(err) {
        return std::process::ExitCode::SUCCESS;
    }
    let code = typed_error(err).map(|e| e.exit_code()).unwrap_or(1);
    if mode != Mode::Pretty {
        match typed_error(err) {
            Some(te) => {
                let _ = output::write_error_json(mode, te);
            }
            None => {
                let envelope = serde_json::json!({
                    "error": {"code": "internal_error", "message": err.to_string()}
                });
                if let Ok(s) = serde_json::to_string(&envelope) {
                    println!("{s}");
                }
            }
        }
    }
    if !quiet {
        eprintln!("webseek: error: {err}");
    }
    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .map(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            .unwrap_or(false)
    })
}

fn typed_error(err: &anyhow::Error) -> Option<&Error> {
    err.chain().find_map(|e| e.downcast_ref::<Error>())
}

/// Handles the config-free commands (`init`, `engines`, `config path`),
/// then loads config and dispatches everything else.
fn run_dispatch(cli: &Cli, mode: Mode) -> anyhow::Result<()> {
    if let Command::Init = &cli.cmd {
        let path = config::Config::write_default(cli.config.as_deref())?;
        if !cli.quiet {
            output::note(&format!("wrote config to {}", path.display()));
        }
        return Ok(());
    }
    if let Command::Engines = &cli.cmd {
        output::write_engines(mode, &engines::catalog())?;
        return Ok(());
    }
    if let Command::Config { action } = &cli.cmd {
        match action {
            ConfigCommand::Path => {
                let path = config::Config::effective_path(cli.config.as_deref());
                output::write_path(mode, "path", &path.display().to_string())?;
            }
        }
        return Ok(());
    }

    let cfg = config::Config::load(cli.config.as_deref())?;

    if let Command::Cache { action } = &cli.cmd {
        let mut cache = build_cache(cli, &cfg);
        match action {
            CacheCommand::Info => {
                output::write_cache_info(mode, &cache.info())?;
            }
            CacheCommand::Clear => {
                let n = cache
                    .clear()
                    .map_err(|e| Error::Config(format!("cannot clear cache: {e}")))?;
                if mode == Mode::Pretty && !cli.quiet {
                    output::note(&format!(
                        "cleared {n} cache entr{}",
                        if n == 1 { "y" } else { "ies" }
                    ));
                } else {
                    output::write_path(mode, "cleared", &n.to_string())?;
                }
            }
        }
        return Ok(());
    }

    let use_color = output::resolve_color(cli.color);
    let http = build_http(cli, &cfg)?;
    let cache = Arc::new(Mutex::new(build_cache(cli, &cfg)));
    let robots = Arc::new(RobotsChecker::new());

    let outcome = run_command(cli, &cfg, &http, mode, use_color, &cache, &robots);
    if let Ok(mut c) = cache.lock() {
        c.save(); // best-effort, even when the command failed
    }
    outcome
}

fn build_cache(cli: &Cli, cfg: &config::Config) -> Cache {
    let enabled = if cli.no_cache {
        false
    } else {
        cli.cache || cfg.cache_max_entries > 0
    };
    if !enabled {
        Cache::disabled()
    } else {
        Cache::load(
            cache::default_cache_path(),
            cfg.cache_ttl_secs,
            cfg.cache_max_entries.max(1),
            cfg.cache_max_bytes,
        )
    }
}

fn build_http(cli: &Cli, cfg: &config::Config) -> crate::error::Result<Http> {
    let policy = net::EgressPolicy {
        allow_private: cli.allow_private || cfg.allow_private_network,
    };
    let allow_proxy = !cli.no_proxy && cfg.allow_proxy;
    // A proxy resolves the destination itself, so the private-network/
    // DNS-rebinding checks below can't see the real target for that hop.
    // Only worth mentioning when that guard is actually doing something —
    // if the caller already opted into --allow-private, there's nothing a
    // proxy could additionally bypass.
    if allow_proxy && !policy.allow_private && !cli.quiet {
        if let Some(var) = net::active_proxy_env() {
            output::note(&format!(
                "${var} is set: requests may go through a proxy, which resolves the \
                 destination itself — the private-network/DNS-rebinding egress checks \
                 cannot see or block the real target for proxied requests. Pass \
                 --no-proxy for the strongest guarantee if you don't need the proxy."
            ));
        }
    }
    let timeout = cli.timeout.map(Duration::from_secs).unwrap_or(cfg.timeout);
    let user_agent = cli.ua.clone().unwrap_or_else(|| cfg.user_agent.clone());
    let accept_language = http::accept_language_for(cfg.lang.as_deref());
    let client = http::build_client(timeout, &user_agent, &accept_language, policy, allow_proxy)?;
    let delay = cli.delay.map(Duration::from_millis).unwrap_or(cfg.delay);
    let limiter = Arc::new(RateLimiter::new(delay));
    Ok(Http::new(client, limiter, policy))
}

fn resolve_bool(explicit_true: bool, explicit_false: bool, config_default: bool) -> bool {
    if explicit_false {
        false
    } else if explicit_true {
        true
    } else {
        config_default
    }
}

#[allow(clippy::too_many_arguments)]
fn run_command(
    cli: &Cli,
    cfg: &config::Config,
    http: &Http,
    mode: Mode,
    use_color: bool,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<RobotsChecker>,
) -> anyhow::Result<()> {
    let verbose = cli.verbose && !cli.quiet;
    let respect_robots = if cli.ignore_robots {
        false
    } else if cli.respect_robots {
        true
    } else {
        cfg.respect_robots
    };
    let fallback = if cli.no_fallback {
        false
    } else if cli.fallback {
        true
    } else {
        cfg.fallback
    };

    match &cli.cmd {
        Command::Init | Command::Engines | Command::Config { .. } | Command::Cache { .. } => {
            unreachable!("handled in run_dispatch")
        }
        Command::Search {
            query,
            count,
            engine,
            lang,
            region,
            safe,
            no_safe,
            open,
        } => {
            let requested = engine.as_deref().unwrap_or(&cfg.engine);
            engines::validate_engine(requested)?;
            let canonical = engines::canonical_name(requested).expect("validated above");
            let opts = SearchOpts {
                count: count.unwrap_or(cfg.max_results).clamp(1, 50),
                lang: lang.clone().or_else(|| cfg.lang.clone()),
                region: region.clone().or_else(|| cfg.region.clone()),
                safe: resolve_bool(*safe, *no_safe, cfg.safe_search),
            };
            if verbose {
                output::note(&format!("searching '{query}' via {canonical}"));
            }
            let (results, eo) =
                search_with_cache(http, canonical, query, &opts, cache, fallback, verbose)?;
            if let Some(idx) = open {
                let i = idx
                    .checked_sub(1)
                    .ok_or_else(|| Error::Usage("--open index must be >= 1".to_string()))?;
                let target = results.get(i).ok_or_else(|| {
                    Error::NoResults(format!("no result #{idx} (only {} found)", results.len()))
                })?;
                open_result_url(&target.url, cli.allow_external_schemes)?;
                return Ok(());
            }
            write_search(mode, use_color, query, &eo, &results)?;
            Ok(())
        }
        Command::Fetch {
            urls,
            max_chars,
            markdown,
            html,
            jobs,
            fail_on_any_error,
            fail_if_all_error,
            open,
        } => {
            if *open {
                if urls.len() != 1 {
                    return Err(Error::Usage("--open requires exactly one URL".to_string()).into());
                }
                open_result_url(&urls[0], cli.allow_external_schemes)?;
                return Ok(());
            }
            let opts = FetchOpts {
                max_bytes: reader::DEFAULT_MAX_BYTES,
                max_chars: max_chars.unwrap_or(cfg.max_chars),
                raw_html: *html,
                markdown: *markdown,
            };

            if urls.len() == 1 {
                fetch_single(
                    http,
                    &urls[0],
                    &opts,
                    mode,
                    use_color,
                    cache,
                    robots,
                    respect_robots,
                    verbose,
                )?;
            } else {
                // More workers than URLs would just spin without work; the
                // CLI parser already enforces jobs <= 64.
                let jobs = jobs.map(|j| j as usize).unwrap_or(1).clamp(1, urls.len());
                if verbose {
                    output::note(&format!(
                        "fetching {} URLs with {jobs} worker(s)",
                        urls.len()
                    ));
                }
                let items =
                    batch::fetch_many(http, urls, &opts, jobs, cache, robots, respect_robots);
                write_fetch_batch(mode, use_color, &items)?;
                let failed = items
                    .iter()
                    .filter(|it| matches!(it, batch::BatchItem::Err { .. }))
                    .count();
                if *fail_on_any_error && failed > 0 {
                    anyhow::bail!("{failed} of {} URLs failed", items.len());
                }
                if *fail_if_all_error && !items.is_empty() && failed == items.len() {
                    anyhow::bail!("all {failed} URLs failed");
                }
            }
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
            no_safe,
            overwrite,
        } => {
            let requested = engine.as_deref().unwrap_or(&cfg.image_engine);
            engines::validate_image_engine(requested)?;
            let canonical = engines::canonical_image_name(requested).expect("validated above");
            let count = count.unwrap_or(cfg.max_results).clamp(1, 50);
            let safe = resolve_bool(*safe, *no_safe, cfg.safe_search);
            if verbose {
                output::note(&format!("searching images for '{query}' via {canonical}"));
            }
            let (results, eo) = images_with_cache(
                http, canonical, query, count, safe, cache, fallback, verbose,
            )?;
            let overwrite = *overwrite || cfg.image_overwrite;
            let downloaded = match download {
                Some(dir) => Some(engines::images::download(
                    http,
                    &results,
                    dir,
                    limit.unwrap_or(results.len()),
                    max_bytes.unwrap_or(cfg.image_max_bytes),
                    overwrite,
                )?),
                None => None,
            };
            write_images(mode, use_color, query, &eo, &results, downloaded)?;
            Ok(())
        }
    }
}

fn open_result_url(raw: &str, allow_external_schemes: bool) -> crate::error::Result<()> {
    let url =
        url::Url::parse(raw).map_err(|e| Error::Config(format!("invalid URL '{raw}': {e}")))?;
    net::check_open_url(&url, allow_external_schemes)?;
    open::that(raw).map_err(|e| Error::Network(format!("cannot open browser: {e}")))
}

/// Single-URL fetch honoring cache and (optionally) robots.txt. Keeps the
/// established single-object JSON contract.
#[allow(clippy::too_many_arguments)]
fn fetch_single(
    http: &Http,
    url: &str,
    opts: &FetchOpts,
    mode: Mode,
    use_color: bool,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<RobotsChecker>,
    respect_robots: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    if respect_robots {
        let allowed = robots.is_allowed(http, url).unwrap_or(true);
        if !allowed {
            return Err(Error::Network(format!(
                "blocked by robots.txt: {url} (use --ignore-robots to override)"
            ))
            .into());
        }
    }
    let key = crate::cache::cache_key(&[
        "fetch",
        url,
        &opts.max_chars.to_string(),
        &opts.raw_html.to_string(),
        &opts.markdown.to_string(),
    ]);
    if let Some(v) = cache.lock().ok().and_then(|mut c| c.get(&key)) {
        match serde_json::from_value::<FetchResult>(v) {
            Ok(fetched) => {
                if verbose {
                    output::note(&format!("{url} (cache hit)"));
                }
                write_fetch(mode, use_color, &fetched)?;
                return Ok(());
            }
            Err(_) => {
                // Corrupt/stale entry: drop and refetch, same as batch mode.
                if let Ok(mut c) = cache.lock() {
                    c.remove(&key);
                }
            }
        }
    }
    if verbose {
        output::note(&format!("fetching {url}"));
    }
    let fetched = reader::fetch(http, url, opts)?;
    if let Ok(mut c) = cache.lock() {
        if let Ok(v) = serde_json::to_value(&fetched) {
            c.put(key, v);
        }
    }
    write_fetch(mode, use_color, &fetched)?;
    Ok(())
}

/// What gets cached for a search: the results *and* which engine actually
/// produced them, so a cache hit can still correctly report a fallback that
/// happened before the entry was written.
#[derive(Serialize, Deserialize)]
struct CachedSearch {
    engine_used: String,
    results: Vec<crate::models::SearchResult>,
}

#[derive(Serialize, Deserialize)]
struct CachedImages {
    engine_used: String,
    results: Vec<crate::models::ImageResult>,
}

/// Search honoring the cache, with automatic same-capability engine fallback.
///
/// A successful fallback is cached under *both* the originally requested
/// engine's key and the engine that actually answered — previously only the
/// latter was written, so the next call for the original engine repeated the
/// failing request before falling back again every single time.
fn search_with_cache(
    http: &Http,
    canonical: &'static str,
    query: &str,
    opts: &SearchOpts,
    cache: &Arc<Mutex<Cache>>,
    fallback_enabled: bool,
    verbose: bool,
) -> anyhow::Result<(Vec<crate::models::SearchResult>, EngineOutcome)> {
    let key_for = |engine: &str| {
        crate::cache::cache_key(&[
            "search",
            engine,
            query,
            &opts.count.to_string(),
            opts.lang.as_deref().unwrap_or(""),
            opts.region.as_deref().unwrap_or(""),
            &opts.safe.to_string(),
        ])
    };
    let requested_key = key_for(canonical);
    if let Some(v) = cache.lock().ok().and_then(|mut c| c.get(&requested_key)) {
        if let Ok(cached) = serde_json::from_value::<CachedSearch>(v) {
            return Ok((
                cached.results,
                EngineOutcome {
                    requested: canonical.to_string(),
                    used: cached.engine_used,
                    cache_hit: true,
                },
            ));
        }
    }

    let order = if fallback_enabled {
        engines::fallback_order(canonical)
    } else {
        Vec::new()
    };
    let order: Vec<&str> = if order.is_empty() {
        vec![canonical]
    } else {
        order
    };

    let mut last_err: Option<Error> = None;
    for eng_name in order {
        let eng = engines::engine_by_name(eng_name)?;
        match eng.search(http, query, opts) {
            Ok(results) => {
                let used = eng.name();
                let envelope = CachedSearch {
                    engine_used: used.to_string(),
                    results: results.clone(),
                };
                if let Ok(v) = serde_json::to_value(&envelope) {
                    if let Ok(mut c) = cache.lock() {
                        c.put(requested_key, v.clone());
                        if used != canonical {
                            c.put(key_for(used), v);
                        }
                    }
                }
                return Ok((
                    results,
                    EngineOutcome {
                        requested: canonical.to_string(),
                        used: used.to_string(),
                        cache_hit: false,
                    },
                ));
            }
            Err(e) => {
                if !e.is_retryable() {
                    return Err(e.into());
                }
                if verbose {
                    output::note(&format!("{eng_name} failed ({e})"));
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("engine order is non-empty").into())
}

/// Image search honoring the cache, with automatic engine fallback. Mirrors
/// [`search_with_cache`]'s dual-key caching.
#[allow(clippy::too_many_arguments)]
fn images_with_cache(
    http: &Http,
    canonical: &'static str,
    query: &str,
    count: usize,
    safe: bool,
    cache: &Arc<Mutex<Cache>>,
    fallback_enabled: bool,
    verbose: bool,
) -> anyhow::Result<(Vec<crate::models::ImageResult>, EngineOutcome)> {
    let key_for = |engine: &str| {
        crate::cache::cache_key(&[
            "images",
            engine,
            query,
            &count.to_string(),
            &safe.to_string(),
        ])
    };
    let requested_key = key_for(canonical);
    if let Some(v) = cache.lock().ok().and_then(|mut c| c.get(&requested_key)) {
        if let Ok(cached) = serde_json::from_value::<CachedImages>(v) {
            return Ok((
                cached.results,
                EngineOutcome {
                    requested: canonical.to_string(),
                    used: cached.engine_used,
                    cache_hit: true,
                },
            ));
        }
    }

    let order = if fallback_enabled {
        engines::fallback_order_images(canonical)
    } else {
        Vec::new()
    };
    let order: Vec<&str> = if order.is_empty() {
        vec![canonical]
    } else {
        order
    };

    let mut last_err: Option<Error> = None;
    for eng_name in order {
        let eng = engines::image_engine_by_name(eng_name)?;
        match eng.search(http, query, count, safe) {
            Ok(results) => {
                let used = eng.name();
                let envelope = CachedImages {
                    engine_used: used.to_string(),
                    results: results.clone(),
                };
                if let Ok(v) = serde_json::to_value(&envelope) {
                    if let Ok(mut c) = cache.lock() {
                        c.put(requested_key, v.clone());
                        if used != canonical {
                            c.put(key_for(used), v);
                        }
                    }
                }
                return Ok((
                    results,
                    EngineOutcome {
                        requested: canonical.to_string(),
                        used: used.to_string(),
                        cache_hit: false,
                    },
                ));
            }
            Err(e) => {
                if !e.is_retryable() {
                    return Err(e.into());
                }
                if verbose {
                    output::note(&format!("{eng_name} failed ({e})"));
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("engine order is non-empty").into())
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
