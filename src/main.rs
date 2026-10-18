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
    input::Inputs,
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
use tokio::sync::OnceCell;

#[derive(Parser)]
#[command(
    version,
    about = "Rust gallery downloader with experimental native Instagram, Facebook, and Twitter/X extractors"
)]
struct Args {
    /// Instagram, Facebook, Twitter/X, or direct media URLs
    #[arg(required_unless_present = "input_files")]
    urls: Vec<String>,
    /// Read URLs from a UTF-8 file; repeat for multiple files, or use - for stdin
    #[arg(short = 'i', long = "input-file", value_name = "FILE")]
    input_files: Vec<PathBuf>,
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
    let inputs = Inputs::new(args.urls.clone(), args.input_files.clone())?;
    let extractors = Extractors::default();
    let archives: BTreeMap<_, OnceCell<Result<Archive>>> = settings
        .values()
        .filter_map(|settings| settings.options.archive.clone())
        .map(|path| (path, OnceCell::new()))
        .collect();
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
    let media = inputs
        .stream()
        .enumerate()
        .flat_map(|(index, source)| {
            let failed = &failed;
            let input = match source {
                Ok(source) => {
                    let selection = extract::http_url(&source).ok().and_then(|url| {
                        settings[extract::social_site(&url).unwrap_or("direct")]
                            .selection
                            .as_ref()
                    });
                    let input =
                        extract_input(source, &extractors, &backend, args.extractor, &settings);
                    match selection {
                        Some(selection) => selection.apply(input),
                        None => input,
                    }
                }
                Err(e) => stream::once(async move { Err(e) }).boxed(),
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
                let archive = settings
                    .options
                    .archive
                    .as_ref()
                    .map(|path| (&archives[path], path));
                let filename = settings.filename.as_ref();
                let options = DownloadOptions {
                    retries: settings.options.retries.unwrap(),
                    write_metadata: args.write_metadata,
                    directory: settings.directory.as_ref(),
                };
                async move {
                    match archive {
                        Some((cell, path)) => {
                            let archive = cell
                                .get_or_init(|| Archive::open(path))
                                .await
                                .as_ref()
                                .map_err(|e| {
                                error(format!("download archive initialization failed: {e}"))
                            })?;
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

#[derive(Default)]
struct Extractors {
    instagram: OnceCell<Result<Instagram>>,
    twitter: OnceCell<Result<Twitter>>,
    facebook: OnceCell<Result<Facebook>>,
}

fn extract_input<'a>(
    source: String,
    extractors: &'a Extractors,
    backend: &'a Backend,
    extractor: Extractor,
    settings: &'a BTreeMap<&str, Settings>,
) -> BoxStream<'a, Result<extract::Media>> {
    stream::once(async move {
        let url = extract::http_url(&source)?;
        let input: BoxStream<'a, Result<extract::Media>> = if let Some(category) =
            extract::social_site(&url)
        {
            let options = &settings[category].options;
            match extractor {
                Extractor::Native if category == "instagram" => {
                    let native = extractors
                        .instagram
                        .get_or_init(|| async {
                            Instagram::new(options.cookies.as_deref(), backend.timeout)
                                .await
                                .map(|native| {
                                    native
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Instagram initialization failed: {e}"))
                        })?;
                    native.stream(&url)
                }
                Extractor::Native if category == "twitter" => {
                    let native = extractors
                        .twitter
                        .get_or_init(|| async {
                            Twitter::new(options.cookies.as_deref(), backend.timeout)
                                .await
                                .map(|native| {
                                    native
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Twitter initialization failed: {e}")))?;
                    stream::once(async move { native.extract(&url).await })
                        .map_ok(|items| stream::iter(items.into_iter().map(Ok)))
                        .try_flatten()
                        .boxed()
                }
                Extractor::Native if category == "facebook" => {
                    let native = extractors
                        .facebook
                        .get_or_init(|| async {
                            Facebook::new(options.cookies.as_deref(), backend.timeout)
                                .await
                                .map(|native| {
                                    native
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Facebook initialization failed: {e}"))
                        })?;
                    stream::once(async move { native.extract(&url).await })
                        .map_ok(|items| stream::iter(items.into_iter().map(Ok)))
                        .try_flatten()
                        .boxed()
                }
                Extractor::Native => {
                    return Err(error(
                        "native extractor unavailable; use --extractor compatibility",
                    ));
                }
                Extractor::Compatibility => {
                    let mut backend = backend.clone();
                    backend.cookies = options.cookies.clone();
                    backend.stream(&url)
                }
            }
        } else if let Some(item) = extract::direct(&url) {
            stream::once(async move { Ok(item) }).boxed()
        } else {
            return Err(error(
                "unsupported URL; expected a social URL or direct media file",
            ));
        };
        Ok(input)
    })
    .try_flatten()
    .boxed()
}

struct Settings {
    options: ConfigOptions,
    filename: Option<FilenameTemplate>,
    directory: Option<DirectoryTemplate>,
    selection: Option<RangeSelection>,
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
            },
        );
    }
    Ok(settings)
}
