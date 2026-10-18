use clap::{Parser, ValueEnum};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use gallery_dl_rs::{
    Result,
    archive::{Archive, ArchiveOutcome},
    config::{Config, Options as ConfigOptions},
    download::{self, DownloadOptions, Outcome},
    error,
    extract::{self, Backend},
    facebook::Facebook,
    instagram::Instagram,
    naming::{DirectoryTemplate, FilenameTemplate},
    selection::RangeSelection,
    twitter::Twitter,
};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Rust gallery downloader with experimental native Instagram, Facebook, and Twitter/X extractors"
)]
struct Args {
    /// Instagram, Facebook, Twitter/X, or direct media URLs
    #[arg(required = true)]
    urls: Vec<String>,
    /// Output root (default: downloads)
    #[arg(short, long)]
    destination: Option<PathBuf>,
    /// Exact download directory, overriding configured folder components
    #[arg(short = 'D', long = "directory", conflicts_with_all = ["destination", "directory"])]
    exact_directory: Option<PathBuf>,
    /// Maximum simultaneous downloads
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=32))]
    jobs: u8,
    /// Select one-based file positions per input (e.g. 5, 8-20, or 1:24:3)
    #[arg(long = "range", allow_hyphen_values = true)]
    file_range: Option<String>,
    /// Print media URLs without downloading (still performs extraction)
    #[arg(short = 'g', long, conflicts_with = "dump_json")]
    get_urls: bool,
    /// Print extracted media as a JSON array without downloading
    #[arg(short = 'j', long)]
    dump_json: bool,
    /// Save post metadata in a JSON sidecar beside each completed media file
    #[arg(long, conflicts_with_all = ["get_urls", "dump_json"])]
    write_metadata: bool,
    /// SQLite history of completed media (gallery-dl-rs format)
    #[arg(long, alias = "archive", conflicts_with_all = ["get_urls", "dump_json"])]
    download_archive: Option<PathBuf>,
    /// Native filename template, e.g. '{post_id}_{index:03}.{extension}'
    #[arg(short = 'f', long, conflicts_with_all = ["get_urls", "dump_json"])]
    filename: Option<String>,
    /// Folder component template; repeat for nested folders (replaces category)
    #[arg(long = "directory-template", allow_hyphen_values = true)]
    directory: Vec<String>,
    /// Retain validated partial HTTP media for retries and later invocations
    #[arg(long, conflicts_with_all = ["get_urls", "dump_json"])]
    resume: bool,
    /// Netscape session cookies for social extraction
    #[arg(short = 'C', long)]
    cookies: Option<PathBuf>,
    /// Social extractor; native supports experimental Instagram feeds/posts, Facebook photos, and X posts
    #[arg(long, value_enum, default_value_t = Extractor::Compatibility)]
    extractor: Extractor,
    /// Explicit gallery-dl configuration file (default config is ignored)
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Explicit native JSON settings (separate from compatibility backend config)
    #[arg(long)]
    native_config: Option<PathBuf>,
    #[arg(long, default_value = "gallery-dl")]
    extractor_command: PathBuf,
    /// Seconds for native extraction or active compatibility reading (excludes backpressure)
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..))]
    extraction_timeout: u64,
    /// Seconds per media attempt/native HTTP request (default: 60)
    #[arg(long, value_parser = parse_timeout)]
    timeout: Option<Duration>,
    /// Additional download/native extraction attempts (default: 2)
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=10))]
    retries: Option<u8>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Extractor {
    Compatibility,
    Native,
}

fn parse_timeout(value: &str) -> std::result::Result<Duration, String> {
    let invalid = || "timeout must be positive finite seconds within the clock range".to_owned();
    let seconds = value.parse::<f64>().map_err(|_| invalid())?;
    let timeout = Duration::try_from_secs_f64(seconds).map_err(|_| invalid())?;
    if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
        return Err(invalid());
    }
    Ok(timeout)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    // Leaving this scope drops the operation before exiting: temporary files
    // are removed and compatibility children use kill_on_drop(true).
    let (result, interrupted) = {
        tokio::select! {
            result = run(args) => (result, false),
            signal = tokio::signal::ctrl_c() => (signal.map_err(Into::into), true),
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    if interrupted {
        eprintln!("cancelled");
        std::process::exit(130);
    }
}

async fn run(args: Args) -> Result<()> {
    if matches!(args.extractor, Extractor::Native) && args.config.is_some() {
        return Err(error(
            "--config is only supported with --extractor compatibility",
        ));
    }
    let settings = resolved_settings(&args).await?;
    let needs_native = |category| {
        matches!(args.extractor, Extractor::Native)
            && args.urls.iter().any(|source| {
                extract::http_url(source)
                    .ok()
                    .is_some_and(|url| extract::social_site(&url) == Some(category))
            })
    };
    let instagram_settings = &settings["instagram"];
    let native = if needs_native("instagram") {
        Some(
            Instagram::new(
                instagram_settings.options.cookies.as_deref(),
                Duration::from_secs(args.extraction_timeout),
            )
            .await
            .map(|native| {
                native
                    .with_retries(instagram_settings.options.retries.unwrap())
                    .with_request_timeout(instagram_settings.options.timeout.unwrap())
            }),
        )
    } else {
        None
    };
    let twitter_settings = &settings["twitter"];
    let twitter = if needs_native("twitter") {
        Some(
            Twitter::new(
                twitter_settings.options.cookies.as_deref(),
                Duration::from_secs(args.extraction_timeout),
            )
            .await
            .map(|twitter| {
                twitter
                    .with_retries(twitter_settings.options.retries.unwrap())
                    .with_request_timeout(twitter_settings.options.timeout.unwrap())
            }),
        )
    } else {
        None
    };
    let facebook_settings = &settings["facebook"];
    let facebook = if needs_native("facebook") {
        Some(
            Facebook::new(
                facebook_settings.options.cookies.as_deref(),
                Duration::from_secs(args.extraction_timeout),
            )
            .await
            .map(|native| {
                native
                    .with_retries(facebook_settings.options.retries.unwrap())
                    .with_request_timeout(facebook_settings.options.timeout.unwrap())
            }),
        )
    } else {
        None
    };
    let backend = Backend {
        executable: args.extractor_command.clone(),
        cookies: None,
        config: args.config.clone(),
        timeout: Duration::from_secs(args.extraction_timeout),
    };
    let failed = Cell::new(0);
    let mut seen = HashSet::new();
    // Pull one input at a time, retaining one parsed message and active downloads.
    // buffer_unordered below supplies backpressure when all download slots are full.
    let media = stream::iter(args.urls.iter().enumerate())
        .flat_map(|(index, source)| {
            let failed = &failed;
            let input = extract_input(
                source,
                native.as_ref(),
                twitter.as_ref(),
                facebook.as_ref(),
                &backend,
                args.extractor,
                &settings,
            );
            let selection = extract::http_url(source).ok().and_then(|url| {
                settings[extract::social_site(&url).unwrap_or("direct")]
                    .selection
                    .as_ref()
            });
            let input = match selection {
                Some(selection) => selection.apply(input),
                None => input,
            };
            input.filter_map(move |result| {
                std::future::ready(match result {
                    Ok(item) => Some(item),
                    Err(e) => {
                        eprintln!("extraction failed for input {}: {e}", index + 1);
                        failed.set(failed.get() + 1);
                        None
                    }
                })
            })
        })
        .filter(|item| {
            // Keep fixed-size digests instead of retaining URLs, headers, or inline manifests.
            let identity: [u8; 32] = Sha256::digest(item.identity().as_bytes()).into();
            std::future::ready(seen.insert(identity))
        });
    tokio::pin!(media);
    if args.dump_json {
        let media: Vec<_> = media.collect().await;
        println!("{}", serde_json::to_string_pretty(&media)?);
    } else if args.get_urls {
        while let Some(item) = media.next().await {
            println!("{}", item.url);
        }
    } else {
        let client = download::client(Duration::from_secs(60))?.with_resume(args.resume);
        let results = media
            .map(|item| {
                let settings = &settings[item.category.as_str()];
                let client = client
                    .clone()
                    .with_timeout(settings.options.timeout.unwrap());
                let destination = settings.options.base_directory.as_ref().unwrap();
                let archive = &settings.archive;
                let filename = settings.filename.as_ref();
                let options = DownloadOptions {
                    retries: settings.options.retries.unwrap(),
                    write_metadata: args.write_metadata,
                    directory: settings.directory.as_ref(),
                };
                async move {
                    match archive {
                        Some(archive) => {
                            archive
                                .download_with_template(
                                    &client,
                                    destination,
                                    &item,
                                    options,
                                    filename,
                                )
                                .await
                        }
                        None => download::download_with_template(
                            &client,
                            destination,
                            &item,
                            options,
                            filename,
                        )
                        .await
                        .map(ArchiveOutcome::Completed),
                    }
                }
            })
            .buffer_unordered(args.jobs as usize);
        tokio::pin!(results);
        while let Some(result) = results.next().await {
            match result {
                Ok(ArchiveOutcome::Completed(Outcome::Downloaded(path))) => {
                    eprintln!("downloaded {}", path.display())
                }
                Ok(ArchiveOutcome::Completed(Outcome::Skipped(path))) => {
                    eprintln!("skipped {}", path.display())
                }
                Ok(ArchiveOutcome::AlreadyArchived) => eprintln!("skipped archived media"),
                Err(e) => {
                    eprintln!("download failed: {e}");
                    failed.set(failed.get() + 1);
                }
            }
        }
    }
    let failed = failed.get();
    if failed > 0 {
        Err(error(format!("{failed} operation(s) failed")))
    } else {
        Ok(())
    }
}

fn extract_input<'a>(
    source: &'a str,
    native: Option<&'a Result<Instagram>>,
    twitter: Option<&'a Result<Twitter>>,
    facebook: Option<&'a Result<Facebook>>,
    backend: &'a Backend,
    extractor: Extractor,
    settings: &'a BTreeMap<&str, Settings>,
) -> BoxStream<'a, Result<extract::Media>> {
    let build = || -> Result<BoxStream<'a, Result<extract::Media>>> {
        let url = extract::http_url(source)?;
        // Social pages use their extractor even when the path has a media suffix.
        if let Some(category) = extract::social_site(&url) {
            match extractor {
                Extractor::Native if category == "instagram" => {
                    let native = native
                        .ok_or_else(|| error("native Instagram extractor was not initialized"))?
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Instagram initialization failed: {e}"))
                        })?;
                    Ok(native.stream(&url))
                }
                Extractor::Native if category == "twitter" => {
                    let twitter = twitter
                        .ok_or_else(|| error("native Twitter extractor was not initialized"))?
                        .as_ref()
                        .map_err(|e| error(format!("native Twitter initialization failed: {e}")))?;
                    Ok(stream::once(async move { twitter.extract(&url).await })
                        .map_ok(|items| stream::iter(items.into_iter().map(Ok)))
                        .try_flatten()
                        .boxed())
                }
                Extractor::Native if category == "facebook" => {
                    let facebook = facebook
                        .ok_or_else(|| error("native Facebook extractor was not initialized"))?
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Facebook initialization failed: {e}"))
                        })?;
                    Ok(stream::once(async move { facebook.extract(&url).await })
                        .map_ok(|items| stream::iter(items.into_iter().map(Ok)))
                        .try_flatten()
                        .boxed())
                }
                Extractor::Native => Err(error(
                    "native extractor unavailable; use --extractor compatibility",
                )),
                Extractor::Compatibility => {
                    let mut backend = backend.clone();
                    backend.cookies = settings[category].options.cookies.clone();
                    Ok(backend.stream(&url))
                }
            }
        } else if let Some(item) = extract::direct(&url) {
            Ok(stream::once(async move { Ok(item) }).boxed())
        } else {
            Err(error(
                "unsupported URL; expected a social URL or direct media file",
            ))
        }
    };
    match build() {
        Ok(items) => items,
        Err(e) => stream::once(async move { Err(e) }).boxed(),
    }
}

struct Settings {
    options: ConfigOptions,
    filename: Option<FilenameTemplate>,
    directory: Option<DirectoryTemplate>,
    selection: Option<RangeSelection>,
    archive: Option<Archive>,
}

async fn resolved_settings(args: &Args) -> Result<BTreeMap<&'static str, Settings>> {
    let config = match &args.native_config {
        Some(path) => Config::load(path).await?,
        None => Config::default(),
    };
    let defaults = ConfigOptions {
        base_directory: Some(PathBuf::from("downloads")),
        retries: Some(2),
        timeout: Some(Duration::from_secs(60)),
        ..ConfigOptions::default()
    };
    let overrides = ConfigOptions {
        base_directory: args
            .exact_directory
            .clone()
            .or_else(|| args.destination.clone()),
        filename: args.filename.clone(),
        file_range: args.file_range.clone(),
        directory: if args.exact_directory.is_some() {
            Some(Vec::new())
        } else {
            (!args.directory.is_empty()).then(|| args.directory.clone())
        },
        cookies: args.cookies.clone(),
        archive: args.download_archive.clone(),
        retries: args.retries,
        timeout: args.timeout,
    };
    let mut settings = BTreeMap::new();
    for category in ["instagram", "twitter", "facebook", "direct"] {
        let options = defaults
            .overlay(&config.resolve(category)?)
            .overlay(&overrides);
        let filename = options
            .filename
            .as_deref()
            .map(FilenameTemplate::parse)
            .transpose()?;
        let directory = options
            .directory
            .as_deref()
            .map(DirectoryTemplate::parse)
            .transpose()?;
        let selection = options
            .file_range
            .as_deref()
            .map(RangeSelection::parse)
            .transpose()?;
        settings.insert(
            category,
            Settings {
                options,
                filename,
                directory,
                selection,
                archive: None,
            },
        );
    }
    // Inspection needs no archive connection or output directory. Open archives
    // only for categories present among valid inputs, and share same-path instances.
    if !args.get_urls && !args.dump_json {
        let mut archives = BTreeMap::new();
        for source in &args.urls {
            let Ok(url) = extract::http_url(source) else {
                continue;
            };
            let category = extract::social_site(&url).unwrap_or("direct");
            let settings = settings.get_mut(category).unwrap();
            if settings
                .selection
                .as_ref()
                .is_some_and(|selection| selection.last_index().is_none())
            {
                continue;
            }
            if let Some(path) = &settings.options.archive {
                let archive = match archives.get(path) {
                    Some(archive) => Archive::clone(archive),
                    None => {
                        let archive = Archive::open(path).await?;
                        archives.insert(path.clone(), archive.clone());
                        archive
                    }
                };
                settings.archive = Some(archive);
            }
        }
    }
    Ok(settings)
}
