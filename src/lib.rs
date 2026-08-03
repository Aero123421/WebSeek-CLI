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
pub mod reader;
pub mod region;
pub mod robots;
pub mod text;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use reqwest::blocking::Client;

use crate::cache::Cache;
use crate::cli::{Cli, Command};
use crate::engines::{engine_by_name, image_engine_by_name};
use crate::error::Error;
use crate::models::{FetchOpts, FetchResult, SearchOpts};
use crate::output::{write_fetch, write_fetch_batch, write_images, write_search, Mode};
use crate::robots::RobotsChecker;

/// Entry point used by `main.rs`.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let mode = output_mode(&cli);

    // `init` must work even when no config exists yet.
    if let Command::Init = &cli.cmd {
        let path = config::Config::write_default(cli.config.as_deref())?;
        if !cli.quiet {
            output::note(&format!("wrote config to {}", path.display()));
        }
        return Ok(());
    }

    // `engines` is config-free: list what's available and exit.
    if let Command::Engines = &cli.cmd {
        output::write_engines(mode, &engines::catalog())?;
        return Ok(());
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

    let outcome = run_inner(&cli, &cfg, &client, &mode, &cache, &robots, delay);
    if let Ok(c) = cache.lock() {
        c.save(); // best-effort, even when the command failed
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    cli: &Cli,
    cfg: &config::Config,
    client: &Client,
    mode: &Mode,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<Mutex<RobotsChecker>>,
    delay: Duration,
) -> Result<()> {
    let respect_robots = cfg.respect_robots || cli.respect_robots;

    match &cli.cmd {
        Command::Init => unreachable!("handled above"),
        Command::Engines => unreachable!("handled above"),
        Command::Search {
            query,
            count,
            engine,
            lang,
            region,
            safe,
            open,
        } => {
            let name = engine.as_deref().unwrap_or(&cfg.engine);
            let opts = SearchOpts {
                count: (*count).clamp(1, 50),
                lang: lang.clone().or_else(|| cfg.lang.clone()),
                region: region.clone().or_else(|| cfg.region.clone()),
                safe: *safe || cfg.safe_search,
            };
            if cli.verbose && !cli.quiet {
                output::note(&format!("searching '{query}' via {name}"));
            }
            let (results, engine_used) =
                search_with_cache(client, name, query, &opts, cache, cli, cfg)?;
            if let Some(idx) = open {
                let i = idx
                    .checked_sub(1)
                    .ok_or_else(|| Error::Config("--open index must be >= 1".to_string()))?;
                let target = results.get(i).ok_or_else(|| {
                    Error::NoResults(format!("no result #{idx} (only {} found)", results.len()))
                })?;
                open::that(&target.url)
                    .map_err(|e| Error::Network(format!("cannot open browser: {e}")))?;
                return Ok(());
            }
            write_search(*mode, query, engine_used, &results)?;
            pause(delay, cli);
            Ok(())
        }
        Command::Fetch {
            urls,
            max_chars,
            markdown,
            html,
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
                max_chars: *max_chars,
                raw_html: *html,
                markdown: *markdown,
            };

            if urls.len() == 1 {
                fetch_single(
                    client,
                    &urls[0],
                    &opts,
                    mode,
                    cache,
                    robots,
                    respect_robots,
                    cli,
                )?;
            } else {
                if cli.verbose && !cli.quiet {
                    output::note(&format!(
                        "fetching {} URLs with {} worker(s)",
                        urls.len(),
                        (*jobs).max(1)
                    ));
                }
                let items = batch::fetch_many(
                    client,
                    urls,
                    &opts,
                    *jobs,
                    delay,
                    cache,
                    robots,
                    respect_robots,
                );
                write_fetch_batch(*mode, &items)?;
            }
            pause(delay, cli);
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
            let name = engine.as_deref().unwrap_or(&cfg.image_engine);
            if cli.verbose && !cli.quiet {
                output::note(&format!("searching images for '{query}' via {name}"));
            }
            let (results, engine_used) = images_with_cache(
                client,
                name,
                query,
                (*count).clamp(1, 50),
                *safe || cfg.safe_search,
                cache,
                cli,
                cfg,
            )?;
            let downloaded = match download {
                Some(dir) => Some(engines::images::download(
                    client,
                    &results,
                    dir,
                    limit.unwrap_or(results.len()),
                    max_bytes.unwrap_or(cfg.image_max_bytes),
                )?),
                None => None,
            };
            write_images(*mode, query, engine_used, &results, downloaded)?;
            pause(delay, cli);
            Ok(())
        }
    }
}

/// Single-URL fetch honoring cache and (optionally) robots.txt. Keeps the
/// established single-object JSON contract.
#[allow(clippy::too_many_arguments)]
fn fetch_single(
    client: &Client,
    url: &str,
    opts: &FetchOpts,
    mode: &Mode,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<Mutex<RobotsChecker>>,
    respect_robots: bool,
    cli: &Cli,
) -> Result<()> {
    if respect_robots {
        let allowed = match robots.lock() {
            Ok(mut c) => c.is_allowed(client, url).unwrap_or(true),
            Err(_) => true,
        };
        if !allowed {
            return Err(Error::Network(format!(
                "blocked by robots.txt: {url} (use --respect-robots off to override)"
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
    if let Some(v) = cache.lock().ok().and_then(|c| c.get(&key)) {
        if let Ok(fetched) = serde_json::from_value::<FetchResult>(v) {
            if cli.verbose && !cli.quiet {
                output::note(&format!("{url} (cache hit)"));
            }
            write_fetch(*mode, &fetched)?;
            return Ok(());
        }
    }
    if cli.verbose && !cli.quiet {
        output::note(&format!("fetching {url}"));
    }
    let fetched = reader::fetch(client, url, opts)?;
    if let Ok(mut c) = cache.lock() {
        if let Ok(v) = serde_json::to_value(&fetched) {
            c.put(key, v);
        }
    }
    write_fetch(*mode, &fetched)?;
    Ok(())
}

/// Search honoring the cache, with automatic engine fallback.
#[allow(clippy::too_many_arguments)]
fn search_with_cache(
    client: &Client,
    name: &str,
    query: &str,
    opts: &SearchOpts,
    cache: &Arc<Mutex<Cache>>,
    cli: &Cli,
    cfg: &config::Config,
) -> Result<(Vec<crate::models::SearchResult>, &'static str)> {
    let key = crate::cache::cache_key(&[
        "search",
        name,
        query,
        &opts.count.to_string(),
        opts.lang.as_deref().unwrap_or(""),
        opts.region.as_deref().unwrap_or(""),
        &opts.safe.to_string(),
    ]);
    if let Some(v) = cache.lock().ok().and_then(|c| c.get(&key)) {
        if let Ok(results) = serde_json::from_value::<Vec<crate::models::SearchResult>>(v) {
            if cli.verbose && !cli.quiet {
                output::note(&format!("{name} results (cache hit)"));
            }
            return Ok((results, engine_by_name(name)?.name()));
        }
    }
    let fallback = cfg.fallback && !cli.no_fallback;
    // Validate the requested engine first: an unknown name is a config error
    // and must surface as-is (no silent fallback).
    engine_by_name(name)?;
    let order = if fallback {
        engines::fallback_order(name, engines::TEXT_ENGINES)
    } else {
        vec![name]
    };
    let mut last_err: Option<Error> = None;
    let mut outcome: Option<(Vec<crate::models::SearchResult>, &'static str)> = None;
    for eng_name in order {
        let eng = engine_by_name(eng_name)?;
        match eng.search(client, query, opts) {
            Ok(r) => {
                outcome = Some((r, eng.name()));
                break;
            }
            Err(e) => {
                let retryable = matches!(
                    e,
                    Error::RateLimited(_) | Error::Network(_) | Error::Parse(_) | Error::Http(_)
                );
                if !retryable {
                    return Err(e.into());
                }
                if cli.verbose && !cli.quiet {
                    output::note(&format!("{eng_name} failed ({e})"));
                }
                last_err = Some(e);
            }
        }
    }
    let (results, engine_used) = match outcome {
        Some(o) => o,
        None => return Err(last_err.expect("engine order is non-empty").into()),
    };
    if let Ok(mut c) = cache.lock() {
        if let Ok(v) = serde_json::to_value(&results) {
            let hit_key = crate::cache::cache_key(&[
                "search",
                engine_used,
                query,
                &opts.count.to_string(),
                opts.lang.as_deref().unwrap_or(""),
                opts.region.as_deref().unwrap_or(""),
                &opts.safe.to_string(),
            ]);
            c.put(hit_key, v);
        }
    }
    Ok((results, engine_used))
}

/// Image search honoring the cache, with automatic engine fallback.
#[allow(clippy::too_many_arguments)]
fn images_with_cache(
    client: &Client,
    name: &str,
    query: &str,
    count: usize,
    safe: bool,
    cache: &Arc<Mutex<Cache>>,
    cli: &Cli,
    cfg: &config::Config,
) -> Result<(Vec<crate::models::ImageResult>, &'static str)> {
    let key =
        crate::cache::cache_key(&["images", name, query, &count.to_string(), &safe.to_string()]);
    if let Some(v) = cache.lock().ok().and_then(|c| c.get(&key)) {
        if let Ok(results) = serde_json::from_value::<Vec<crate::models::ImageResult>>(v) {
            if cli.verbose && !cli.quiet {
                output::note(&format!("{name} image results (cache hit)"));
            }
            return Ok((results, image_engine_by_name(name)?.name()));
        }
    }
    let fallback = cfg.fallback && !cli.no_fallback;
    image_engine_by_name(name)?;
    let order = if fallback {
        engines::fallback_order(name, engines::IMAGE_ENGINES)
    } else {
        vec![name]
    };
    let mut last_err: Option<Error> = None;
    let mut outcome: Option<(Vec<crate::models::ImageResult>, &'static str)> = None;
    for eng_name in order {
        let eng = image_engine_by_name(eng_name)?;
        match eng.search(client, query, count, safe) {
            Ok(r) => {
                outcome = Some((r, eng.name()));
                break;
            }
            Err(e) => {
                let retryable = matches!(
                    e,
                    Error::RateLimited(_) | Error::Network(_) | Error::Parse(_) | Error::Http(_)
                );
                if !retryable {
                    return Err(e.into());
                }
                if cli.verbose && !cli.quiet {
                    output::note(&format!("{eng_name} failed ({e})"));
                }
                last_err = Some(e);
            }
        }
    }
    let (results, engine_used) = match outcome {
        Some(o) => o,
        None => return Err(last_err.expect("engine order is non-empty").into()),
    };
    if let Ok(mut c) = cache.lock() {
        if let Ok(v) = serde_json::to_value(&results) {
            let hit_key = crate::cache::cache_key(&[
                "images",
                engine_used,
                query,
                &count.to_string(),
                &safe.to_string(),
            ]);
            c.put(hit_key, v);
        }
    }
    Ok((results, engine_used))
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

/// Gentleness: keep a pause between upstream requests unless disabled.
fn pause(delay: Duration, cli: &Cli) {
    if !delay.is_zero() && !cli.quiet {
        thread::sleep(delay);
    }
}
