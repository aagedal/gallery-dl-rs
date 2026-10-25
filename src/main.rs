use clap::{Parser, ValueEnum};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use gallery_dl_rs::{
    Result,
    archive::{Archive, ArchiveOutcome},
    browser::FirefoxProfile,
    config::{Config, Options as ConfigOptions},
    cookies::CookieJar,
    date::source_date_bound,
    download::{self, DownloadOptions, Outcome},
    error,
    event::{self, ExtractionEvent},
    extract::{self, Backend, BackendOptions},
    facebook::Facebook,
    filter::FilterExpression,
    input::Inputs,
    instagram::Instagram,
    job_selection::JobSelection,
    naming::{DirectoryTemplate, FilenameTemplate},
    network::NetworkOptions,
    post_selection::PostSelection,
    selection::RangeSelection,
    size::{SizeLimits, parse_bytes},
    twitter::{
        CardOptions, ConversationMode, ExpansionOptions, MediaOptions, ProfileOptions,
        StatusOptions, ThreadOptions, TimelineOptions, Twitter,
    },
};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::Arc,
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
    #[arg(required_unless_present_any = ["input_files", "list_extractors", "list_modules"])]
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
    /// Select one-based file positions per extractor job (e.g. 5, 8-20, or 1:24:3)
    #[arg(
        long = "range",
        visible_alias = "file-range",
        alias = "image-range",
        allow_hyphen_values = true
    )]
    file_range: Option<String>,
    /// Select files with a metadata expression; repeat to require all expressions
    #[arg(
        long = "filter",
        visible_alias = "file-filter",
        alias = "image-filter",
        value_name = "EXPR",
        allow_hyphen_values = true
    )]
    file_filter: Vec<String>,
    /// Select one-based post positions per extractor job
    #[arg(long = "post-range", allow_hyphen_values = true)]
    post_range: Option<String>,
    /// Select posts before file filtering; repeat to require all expressions
    #[arg(long = "post-filter", value_name = "EXPR", allow_hyphen_values = true)]
    post_filter: Vec<String>,
    /// Select one-based child extractor positions within each parent job
    #[arg(
        long = "child-range",
        alias = "chapter-range",
        allow_hyphen_values = true
    )]
    child_range: Option<String>,
    /// Select child extractors before requests; repeat to require all expressions
    #[arg(
        long = "child-filter",
        alias = "chapter-filter",
        value_name = "EXPR",
        allow_hyphen_values = true
    )]
    child_filter: Vec<String>,
    /// Emit metadata for native X posts without media
    #[arg(long, conflicts_with = "no_text_tweets")]
    text_tweets: bool,
    /// Disable configured X text-post output
    #[arg(long, conflicts_with = "text_tweets")]
    no_text_tweets: bool,
    /// Process posts created before this UTC date or Unix timestamp (exclusive)
    #[arg(long, value_name = "DATE", allow_hyphen_values = true)]
    date_before: Option<String>,
    /// Stop this input at a post on or before this UTC date or Unix timestamp
    #[arg(long, value_name = "DATE", allow_hyphen_values = true)]
    date_after: Option<String>,
    /// Minimum media size in bytes or binary units, e.g. 500k or 2.5M
    #[arg(long, value_parser = parse_size)]
    filesize_min: Option<u64>,
    /// Maximum media size in bytes or binary units (0 disables this bound)
    #[arg(long, value_parser = parse_size)]
    filesize_max: Option<u64>,
    /// Print media URLs without downloading (still performs extraction)
    #[arg(
        short = 'g',
        long,
        visible_alias = "resolve-urls",
        short_alias = 'G',
        conflicts_with = "dump_json"
    )]
    get_urls: bool,
    /// Print extracted media as a JSON array without downloading
    #[arg(short = 'j', long, visible_alias = "resolve-json", short_alias = 'J')]
    dump_json: bool,
    /// Save post metadata in a JSON sidecar beside each completed media file
    #[arg(long, conflicts_with_all = ["get_urls", "dump_json"])]
    write_metadata: bool,
    /// Do not set new files' modification times from HTTP Last-Modified
    #[arg(long)]
    no_mtime: bool,
    /// Set new files' modification times from a metadata date field, e.g. date
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "date")]
    mtime: Option<String>,
    /// Run a shell command after each downloaded file; {} is the quoted path
    #[arg(long, value_name = "CMD", conflicts_with_all = ["get_urls", "dump_json"])]
    exec: Vec<String>,
    /// Run a shell command once after all downloads finish
    #[arg(long, value_name = "CMD", conflicts_with_all = ["get_urls", "dump_json"])]
    exec_after: Vec<String>,
    /// Maximum download rate in bytes per second, e.g. 500k or 2.5M
    #[arg(short = 'r', long, value_parser = parse_size)]
    limit_rate: Option<u64>,
    /// Seconds to wait before each download, as SECONDS or MIN-MAX
    #[arg(long, value_name = "SECONDS", value_parser = parse_sleep)]
    sleep: Option<gallery_dl_rs::sleep::SleepRange>,
    /// Stop the current input after N consecutive skipped (existing or archived) files
    #[arg(short = 'A', long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    abort: Option<u64>,
    /// Stop all inputs after N consecutive skipped (existing or archived) files
    #[arg(short = 'T', long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    terminate: Option<u64>,
    /// SQLite history of completed media (gallery-dl-rs format)
    #[arg(long, alias = "archive", conflicts_with_all = ["get_urls", "dump_json"])]
    download_archive: Option<PathBuf>,
    /// Filename template, e.g. '{date:%Y%m%d}_{post_id}_{index:>02}.{extension}'
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
    /// Browser session: native accepts firefox:PROFILE_PATH; compatibility delegates
    #[arg(long, conflicts_with = "cookies")]
    cookies_from_browser: Option<String>,
    /// HTTP(S) proxy URL; an empty string disables environment proxies
    #[arg(long)]
    proxy: Option<String>,
    /// Override the User-Agent for extraction and media HTTP requests
    #[arg(short = 'a', long)]
    user_agent: Option<String>,
    /// Social extractor; native supports experimental Instagram collections/stories/posts, Facebook media/albums, and X posts/profile tabs
    #[arg(long, value_enum, default_value_t = Extractor::Compatibility)]
    extractor: Extractor,
    /// Explicit gallery-dl configuration file (default config is ignored)
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Explicit native JSON settings (separate from compatibility backend config)
    #[arg(long)]
    native_config: Option<PathBuf>,
    /// Do not load the discovered default native configuration file
    #[arg(long, conflicts_with = "native_config")]
    native_config_ignore: bool,
    /// Additional native setting KEY=VALUE (VALUE is JSON or text), e.g.
    /// extractor.4chan.retries=3; `extractor.*` keys also reach the
    /// compatibility backend
    #[arg(short = 'o', long = "option", value_name = "KEY=VALUE")]
    options: Vec<String>,
    /// Simulate: print destination paths ("# PATH") without downloading
    #[arg(short = 's', long, visible_alias = "no-download", conflicts_with_all = ["get_urls", "dump_json"])]
    simulate: bool,
    /// Print available keywords and example values for the first file of each input
    #[arg(short = 'K', long, conflicts_with_all = ["get_urls", "dump_json", "simulate"])]
    list_keywords: bool,
    /// Do not print downloaded/skipped file paths
    #[arg(short = 'q', long)]
    quiet: bool,
    /// Append input URLs that returned an error to FILE
    #[arg(short = 'e', long, value_name = "FILE")]
    error_file: Option<PathBuf>,
    /// Print supported extractor categories and their subcategories
    #[arg(long)]
    list_extractors: bool,
    /// Print supported extractor categories
    #[arg(long)]
    list_modules: bool,
    /// Seconds to wait before starting extraction for each input (SECONDS or MIN-MAX)
    #[arg(long, value_name = "SECONDS", value_parser = parse_sleep)]
    sleep_extractor: Option<gallery_dl_rs::sleep::SleepRange>,
    /// Activate a native postprocessor by name; repeatable
    #[arg(short = 'P', long = "postprocessor", value_name = "NAME")]
    postprocessor: Vec<String>,
    /// Write gallery metadata to an info.json file per directory
    #[arg(long, alias = "write-infojson")]
    write_info_json: bool,
    /// Write tags to a separate text file per downloaded file
    #[arg(long)]
    write_tags: bool,
    #[arg(long, default_value = "gallery-dl")]
    extractor_command: PathBuf,
    /// yt-dlp executable for video-page and HLS/DASH downloads
    #[arg(long, default_value = "yt-dlp", value_name = "PATH")]
    ytdl_command: PathBuf,
    /// Never run yt-dlp; video-page and HLS/DASH jobs then fail or use fallbacks
    #[arg(long)]
    no_ytdl: bool,
    /// Seconds for native extraction or active compatibility reading (excludes backpressure)
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..))]
    extraction_timeout: u64,
    /// Seconds per media attempt/native HTTP request (default: 60)
    #[arg(long, visible_alias = "http-timeout", value_parser = parse_timeout)]
    timeout: Option<Duration>,
    /// Additional download/native extraction attempts (default: 2)
    #[arg(short = 'R', long, value_parser = clap::value_parser!(u8).range(0..=10))]
    retries: Option<u8>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Extractor {
    Compatibility,
    Native,
}

fn parse_size(value: &str) -> std::result::Result<u64, String> {
    parse_bytes(value).map_err(|e| e.to_string())
}

/// The first existing default native configuration, like upstream's search
/// of user configuration directories. Upstream gallery-dl files are not read:
/// the native schema is a strict subset and would reject their settings.
fn default_native_config() -> Option<PathBuf> {
    let var = |name| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let mut candidates = Vec::new();
    if let Some(path) = var("GALLERY_DL_RS_CONFIG") {
        candidates.push(path);
    }
    if cfg!(windows) {
        candidates.extend(var("APPDATA").map(|dir| dir.join("gallery-dl-rs").join("config.json")));
        candidates.extend(var("USERPROFILE").map(|dir| dir.join("gallery-dl-rs.conf")));
    } else {
        let config =
            var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|home| home.join(".config")));
        candidates.extend(config.map(|dir| dir.join("gallery-dl-rs").join("config.json")));
        candidates.extend(var("HOME").map(|home| home.join(".gallery-dl-rs.conf")));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Compatibility mode keeps plain Imgur file links as direct downloads, as
/// before native Imgur support; native mode extracts them for metadata.
/// Plain `i.redd.it` file links carry no submission context and always stay
/// direct.
fn routed_site(url: &url::Url, extractor: Extractor) -> Option<&'static str> {
    extract::social_site(url).filter(|site| {
        let file = extract::direct(url).is_some();
        !(*site == "imgur"
            && matches!(extractor, Extractor::Compatibility)
            && url.host_str() == Some("i.imgur.com")
            && file
            || *site == "reddit" && url.host_str() == Some("i.redd.it") && file
            || (gallery_dl_rs::booru::Site::from_category(site).is_some()
                || matches!(
                    *site,
                    "mastodon"
                        | "kemono"
                        | "coomer"
                        | "pixeldrain"
                        | "catbox"
                        | "imgbox"
                        | "postimg"
                ))
                && file)
    })
}

fn settings_category(url: &url::Url, extractor: Extractor) -> &'static str {
    match routed_site(url, extractor) {
        Some("mastodon") => {
            gallery_dl_rs::mastodon::instance_category(url.host_str().unwrap()).unwrap()
        }
        Some(category) => category,
        None => "direct",
    }
}

fn booru_subcategory(url: &url::Url) -> Option<&'static str> {
    let category = extract::social_site(url)?;
    let site = gallery_dl_rs::booru::Site::from_category(category)?;
    match gallery_dl_rs::booru::target(site, url).ok()?.0 {
        gallery_dl_rs::booru::Target::Artist { .. } => Some("artist"),
        gallery_dl_rs::booru::Target::ArtistSearch { .. } => Some("artist-search"),
        gallery_dl_rs::booru::Target::Tags { .. } if matches!(category, "danbooru" | "e621") => {
            Some("tag")
        }
        _ => None,
    }
}
fn class_key(category: &str, subcategory: &str) -> Option<&'static str> {
    match (category, subcategory) {
        ("danbooru", "artist") => Some("danbooru.artist"),
        ("danbooru", "artist-search") => Some("danbooru.artist-search"),
        ("danbooru", "tag") => Some("danbooru.tag"),
        ("e621", "artist") => Some("e621.artist"),
        ("e621", "artist-search") => Some("e621.artist-search"),
        ("e621", "tag") => Some("e621.tag"),
        _ => None,
    }
}
fn settings_key(url: &url::Url, extractor: Extractor) -> &'static str {
    let category = settings_category(url, extractor);
    if matches!(extractor, Extractor::Native) {
        booru_subcategory(url)
            .and_then(|class| class_key(category, class))
            .unwrap_or(category)
    } else {
        category
    }
}
fn media_settings_key<'a>(
    media: &'a extract::Media,
    extractor: Extractor,
    settings: &BTreeMap<&str, Settings>,
) -> &'a str {
    if !settings.contains_key(media.category.as_str()) {
        // Generic compatibility output from other gallery-dl extractors.
        return "direct";
    }
    media_settings_key_known(media, extractor)
}
fn media_settings_key_known(media: &extract::Media, extractor: Extractor) -> &str {
    if matches!(extractor, Extractor::Native)
        && media.metadata.as_ref().is_some_and(|metadata| {
            metadata.booru_artist_parent.is_some()
                || gallery_dl_rs::source_archive::for_media(media)
                    .is_some_and(|descriptor| descriptor.format == "t_{search_tags}_{id}")
        })
    {
        class_key(&media.category, "tag").unwrap_or(&media.category)
    } else {
        &media.category
    }
}

fn parse_sleep(value: &str) -> std::result::Result<gallery_dl_rs::sleep::SleepRange, String> {
    gallery_dl_rs::sleep::SleepRange::parse(value).map_err(|e| e.to_string())
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

/// Stack for the thread that polls the top-level job future. Windows gives
/// the main thread only 1 MiB, which the nested extractor/download futures
/// exceed; this matches the common Unix main-thread default.
const MAIN_STACK_SIZE: usize = 8 * 1024 * 1024;

fn main() {
    let args = Args::parse();
    let worker = std::thread::Builder::new()
        .name("main-runtime".into())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build the async runtime")
                .block_on(async_main(args))
        })
        .expect("failed to start the main runtime thread");
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

async fn async_main(args: Args) {
    // Leaving this scope drops the operation before exiting: temporary files
    // are removed and compatibility children use kill_on_drop(true).
    let (result, interrupted) = {
        tokio::select! {
            result = Box::pin(run(args)) => (result, false),
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
    if args.list_extractors || args.list_modules {
        list_extractors(args.list_extractors);
        return Ok(());
    }
    if matches!(args.extractor, Extractor::Native) && args.config.is_some() {
        return Err(error(
            "--config is only supported with --extractor compatibility",
        ));
    }
    // Settings, extractors and the backend live for the rest of the process,
    // so registry sites can route child URLs through them.
    let settings: &'static _ = Box::leak(Box::new(resolved_settings(&args).await?));
    let inputs = Inputs::new(args.urls.clone(), args.input_files.clone())?;
    let extractors: &'static Extractors = Box::leak(Box::new(Extractors {
        ytdl: args.ytdl_command.clone(),
        ytdl_cookies: ytdl_cookies(&args),
        ..Extractors::default()
    }));
    let archives: BTreeMap<_, OnceCell<Result<Archive>>> = settings
        .values()
        .filter_map(|settings| settings.options.archive.clone())
        .map(|path| (path, OnceCell::new()))
        .collect();
    let backend: &'static Backend = Box::leak(Box::new(Backend {
        executable: args.extractor_command.clone(),
        cookies: None,
        config: args.config.clone(),
        timeout: Duration::from_secs(args.extraction_timeout),
    }));
    let extractor = args.extractor;
    gallery_dl_rs::sites::set_router(gallery_dl_rs::sites::Router {
        supports: Box::new(move |url| routable(url, extractor, settings)),
        extract: Box::new(move |url| child_queue(extractors, backend, extractor, settings)(url)),
    });
    let failed = Cell::new(0);
    // Input URLs by index, and the inputs that failed, for -e/--error-file.
    let input_sources = std::cell::RefCell::new(BTreeMap::<usize, String>::new());
    let failed_inputs = std::cell::RefCell::new(std::collections::BTreeSet::<usize>::new());
    // Inputs stopped by -A/--abort; their remaining media are neither
    // extracted nor downloaded.
    let aborted = std::sync::Mutex::new(HashSet::<usize>::new());
    let terminated = std::sync::atomic::AtomicBool::new(false);
    let mut seen = HashSet::new();
    // Pull one input at a time, retaining one parsed message and active downloads.
    // buffer_unordered below supplies backpressure when all download slots are full.
    let job_configurations = std::sync::Arc::new(if args.dump_json || args.get_urls {
        BTreeMap::new()
    } else {
        settings
            .iter()
            .filter_map(|(category, settings)| {
                settings.postprocessors.as_ref().map(|processors| {
                    (
                        (*category).to_owned(),
                        gallery_dl_rs::postprocess::jobs::JobConfiguration {
                            processors: processors.clone(),
                            root: settings.options.base_directory.clone().unwrap(),
                            filename: settings.filename.clone(),
                            directory: settings.directory.clone(),
                            init: settings.options.init.unwrap_or(false),
                        },
                    )
                })
            })
            .collect()
    });
    let media = inputs
        .stream()
        .take_while(|_| std::future::ready(!terminated.load(std::sync::atomic::Ordering::Acquire)))
        .enumerate()
        .flat_map(|(index, source)| {
            if args.error_file.is_some()
                && let Ok(source) = &source
            {
                input_sources.borrow_mut().insert(index, source.clone());
            }
            let failed_inputs = &failed_inputs;
            let failed = &failed;
            let aborted = &aborted;
            let terminated = &terminated;
            let root_category = source
                .as_ref()
                .ok()
                .and_then(|source| extract::http_url(source).ok())
                .map(|url| settings_category(&url, args.extractor).to_owned());
            let root_subcategory = if matches!(args.extractor, Extractor::Native) {
                source
                    .as_ref()
                    .ok()
                    .and_then(|source| extract::http_url(source).ok())
                    .and_then(|url| booru_subcategory(&url))
                    .map(str::to_owned)
            } else {
                None
            };
            let archive_descriptor = source
                .as_ref()
                .ok()
                .and_then(|source| extract::http_url(source).ok())
                .and_then(|url| gallery_dl_rs::source_archive::for_url(&url));
            let input = match source {
                Ok(source) => {
                    let unordered_dates = extract::http_url(&source).ok().is_some_and(|url| {
                        gallery_dl_rs::instagram::is_story_url(&url)
                            || gallery_dl_rs::instagram::is_unordered_collection_url(&url)
                            || gallery_dl_rs::instagram::is_unordered_user_url(
                                &url,
                                settings["instagram"].options.include.as_ref(),
                            )
                            || gallery_dl_rs::twitter::is_profile_timeline_url(&url)
                            || gallery_dl_rs::twitter::is_search_url(&url)
                            || gallery_dl_rs::twitter::is_collection_url(&url)
                            || gallery_dl_rs::twitter::is_queue_url(&url)
                            || matches!(
                                extract::social_site(&url),
                                Some("bluesky" | "kemono" | "coomer")
                            )
                            || matches!(
                                gallery_dl_rs::pixiv::target(&url),
                                Ok(gallery_dl_rs::pixiv::Target::Bookmarks { .. }
                                    | gallery_dl_rs::pixiv::Target::Ranking(_)
                                    | gallery_dl_rs::pixiv::Target::Search(_)
                                    | gallery_dl_rs::pixiv::Target::Series { .. }
                                    | gallery_dl_rs::pixiv::Target::Pixivision(_)
                                    | gallery_dl_rs::pixiv::Target::Sketch(_)
                                    | gallery_dl_rs::pixiv::Target::Novel(
                                        gallery_dl_rs::pixiv::novel::Target::Bookmarks { .. }
                                    ))
                            )
                            || matches!(
                                gallery_dl_rs::mastodon::target(&url),
                                Ok(gallery_dl_rs::mastodon::Target::Collection { .. })
                            )
                            || (extract::social_site(&url) == Some("twitter")
                                && (settings["twitter"].options.quoted == Some(true)
                                    || settings["twitter"].options.expand == Some(true)
                                    || (gallery_dl_rs::twitter::post_id(&url).is_ok()
                                        && matches!(
                                            settings["twitter"].options.conversations,
                                            Some(
                                                ConversationMode::All
                                                    | ConversationMode::Accessible
                                            )
                                        ))))
                    });
                    let selected_settings = extract::http_url(&source)
                        .ok()
                        .map(|url| &settings[settings_key(&url, args.extractor)]);
                    let input =
                        extract_input(source, extractors, backend, args.extractor, settings);
                    let input = match args.sleep_extractor {
                        Some(delay) => stream::once(async move {
                            tokio::time::sleep(delay.sample()).await;
                            input
                        })
                        .flatten()
                        .boxed(),
                        None => input,
                    };
                    match selected_settings {
                        Some(selected) => {
                            let classes = if matches!(
                                root_subcategory.as_deref(),
                                Some("artist" | "artist-search")
                            ) {
                                settings
                                    .iter()
                                    .filter(|(key, _)| {
                                        key.starts_with("danbooru.") || key.starts_with("e621.")
                                    })
                                    .map(|(key, settings)| {
                                        ((*key).to_owned(), settings.job_selection(false))
                                    })
                                    .collect()
                            } else {
                                BTreeMap::new()
                            };
                            selected
                                .job_selection(unordered_dates)
                                .apply_events_with_classes(input, Arc::new(classes))
                        }
                        None => input,
                    }
                }
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            };
            let input = input
                .take_while(move |_| {
                    std::future::ready(
                        !terminated.load(std::sync::atomic::Ordering::Acquire)
                            && !aborted
                                .lock()
                                .expect("abort state lock poisoned")
                                .contains(&index),
                    )
                })
                .boxed();
            gallery_dl_rs::postprocess::jobs::apply_with_source_class(
                input,
                job_configurations.clone(),
                root_category,
                root_subcategory,
                archive_descriptor,
            )
            .filter_map(move |result| {
                std::future::ready(match result {
                    Ok(item) => Some((index, item)),
                    Err(e) => {
                        eprintln!("extraction failed for input {}: {e}", index + 1);
                        failed.set(failed.get() + 1);
                        failed_inputs.borrow_mut().insert(index);
                        None
                    }
                })
            })
        })
        .map(|(index, mut job)| {
            settings[media_settings_key(&job.media, args.extractor, settings)]
                .network
                .apply_media(&mut job.media);
            (index, job)
        })
        .filter(|(_, job)| {
            let item = &job.media;
            if item.category == "twitter" && settings["twitter"].options.unique == Some(false) {
                return std::future::ready(true);
            }
            let mut identity = item.identity();
            // Distinct social files can share a URL and request context.
            if (matches!(
                item.category.as_str(),
                "instagram"
                    | "twitter"
                    | "facebook"
                    | "imgur"
                    | "bluesky"
                    | "reddit"
                    | "danbooru"
                    | "e621"
                    | "gelbooru"
                    | "safebooru"
                    | "mastodon"
                    | "mastodon.social"
                    | "pawoo"
                    | "baraag"
                    | "kemono"
                    | "coomer"
                    | "pixeldrain"
                    | "catbox"
                    | "imgbox"
                    | "postimg"
                    | "tumblr"
                    | "pixiv"
                    | "pixiv-novel"
                    | "deviantart"
                    | "flickr"
                    | "wallhaven"
                    | "artstation"
                    | "pinterest"
                    | "newgrounds"
                    | "furaffinity"
                    | "weasyl"
                    | "vsco"
            ) || gallery_dl_rs::sites::is_category(&item.category))
                && let Some(file_key) = item.archive_key()
            {
                identity = serde_json::json!([file_key, identity]).to_string();
            }
            // Retain only fixed-size digests, without URLs, headers, or manifests.
            let digest: [u8; 32] = Sha256::digest(identity.as_bytes()).into();
            std::future::ready(seen.insert(digest))
        });
    tokio::pin!(media);
    if args.list_keywords {
        let mut shown = HashSet::new();
        while let Some((index, job)) = media.next().await {
            if !shown.insert(index) {
                continue;
            }
            let keywords = gallery_dl_rs::postprocess::public_keywords(
                &gallery_dl_rs::postprocess::context(&job.media),
            );
            let mut out = String::from(
                "Keywords for filenames and --filter:\n------------------------------------\n",
            );
            print_keywords(&mut out, &keywords, "");
            println!("{out}");
            // One file per input suffices; stop its extraction.
            aborted
                .lock()
                .expect("abort state lock poisoned")
                .insert(index);
        }
    } else if args.simulate {
        while let Some((_, job)) = media.next().await {
            let item = &job.media;
            let settings = &settings[media_settings_key(item, args.extractor, settings)];
            match download::destination_with_templates(
                settings.options.base_directory.as_ref().unwrap(),
                item,
                settings.filename.as_ref(),
                settings.directory.as_ref(),
            ) {
                Ok(path) => println!("# {}", path.display()),
                Err(e) => {
                    eprintln!("simulation failed: {e}");
                    failed.set(failed.get() + 1);
                }
            }
        }
    } else if args.dump_json {
        let media: Vec<_> = media.map(|(_, job)| job.media).collect().await;
        println!("{}", serde_json::to_string_pretty(&media)?);
    } else if args.get_urls {
        while let Some((_, item)) = media.next().await {
            println!("{}", item.media.url);
        }
    } else {
        let client = download::client(Duration::from_secs(60))?
            .with_resume(args.resume)
            .with_http_mtime(!args.no_mtime)
            .with_metadata_mtime(args.mtime.clone())
            .with_sleep(args.sleep)
            .with_rate_limit(args.limit_rate)
            .with_ytdl((!args.no_ytdl).then_some(args.ytdl_command.as_path()))
            .with_ytdl_cookies(ytdl_cookies(&args));
        let aborted = &aborted;
        let terminated = &terminated;
        let results = media
            .map(|(index, job)| {
                let gallery_dl_rs::postprocess::jobs::JobMedia { media: item, scope } = job;
                let settings = &settings[media_settings_key(&item, args.extractor, settings)];
                let client = client
                    .clone()
                    .with_timeout(settings.options.timeout.unwrap())
                    .with_fallback(settings.options.fallback.unwrap_or(true));
                let cell = &settings.download_client;
                let destination = settings.options.base_directory.as_ref().unwrap();
                let archive = settings
                    .options
                    .archive
                    .as_ref()
                    .map(|path| (&archives[path], path));
                let filename = settings.filename.as_ref();
                let options = DownloadOptions {
                    postprocessor_scope: None,
                    postprocessors: settings.postprocessors.as_deref(),
                    retries: settings.options.retries.unwrap(),
                    limits: settings.limits,
                    write_metadata: args.write_metadata,
                    directory: settings.directory.as_ref(),
                };
                async move {
                    // Downloads queued before an abort are dropped unstarted.
                    if terminated.load(std::sync::atomic::Ordering::Acquire)
                        || aborted
                            .lock()
                            .expect("abort state lock poisoned")
                            .contains(&index)
                    {
                        return (index, None);
                    }
                    let result: Result<ArchiveOutcome> = async {
                        let options = DownloadOptions {
                            postprocessor_scope: scope.as_ref(),
                            ..options
                        };
                        let client = cell
                            .get_or_init(|| async { client.with_network(&settings.network) })
                            .await
                            .as_ref()
                            .map_err(|e| {
                                error(format!("media HTTP client initialization failed: {e}"))
                            })?;
                        match archive {
                            Some((cell, path)) => {
                                let archive = cell
                                    .get_or_init(|| Archive::open(path))
                                    .await
                                    .as_ref()
                                    .map_err(|e| {
                                        error(format!(
                                            "download archive initialization failed: {e}"
                                        ))
                                    })?;
                                archive
                                    .download_with_template(
                                        client,
                                        destination,
                                        &item,
                                        options,
                                        filename,
                                    )
                                    .await
                            }
                            None => download::download_with_template(
                                client,
                                destination,
                                &item,
                                options,
                                filename,
                            )
                            .await
                            .map(ArchiveOutcome::Completed),
                        }
                    }
                    .await;
                    if result.is_err()
                        && let Some(scope) = &scope
                    {
                        scope.fail();
                    }
                    (index, Some(result))
                }
            })
            .buffer_unordered(args.jobs as usize);
        tokio::pin!(results);
        let mut skipped = 0u64;
        let mut input_skipped = BTreeMap::<usize, u64>::new();
        while let Some((index, result)) = results.next().await {
            let Some(result) = result else { continue };
            let input = input_skipped.entry(index).or_default();
            if matches!(
                result,
                Ok(ArchiveOutcome::Completed(Outcome::Skipped(_))
                    | ArchiveOutcome::AlreadyArchived)
            ) {
                skipped += 1;
                *input += 1;
            } else if matches!(
                result,
                Ok(ArchiveOutcome::Completed(Outcome::Downloaded(_)))
            ) {
                skipped = 0;
                *input = 0;
            }
            if args.abort.is_some_and(|limit| *input >= limit)
                && aborted
                    .lock()
                    .expect("abort state lock poisoned")
                    .insert(index)
            {
                eprintln!(
                    "stopping input {} after {input} consecutive skipped files",
                    index + 1
                );
            }
            match result {
                Ok(ArchiveOutcome::Completed(Outcome::Downloaded(path))) => {
                    if !args.quiet {
                        eprintln!("downloaded {}", path.display());
                    }
                    for command in &args.exec {
                        if let Err(e) = gallery_dl_rs::exec::run(command, Some(&path)).await {
                            eprintln!("postprocessing failed for {}: {e}", path.display());
                            failed.set(failed.get() + 1);
                        }
                    }
                }
                Ok(ArchiveOutcome::Completed(Outcome::Skipped(path))) => {
                    if !args.quiet {
                        eprintln!("skipped {}", path.display())
                    }
                }
                Ok(ArchiveOutcome::Completed(Outcome::Filtered(reason))) => {
                    eprintln!("filtered media: {reason}")
                }
                Ok(ArchiveOutcome::AlreadyArchived) => eprintln!("skipped archived media"),
                Err(e) => {
                    eprintln!("download failed: {e}");
                    failed.set(failed.get() + 1);
                    failed_inputs.borrow_mut().insert(index);
                }
            }
            if args.terminate.is_some_and(|limit| skipped >= limit)
                && !terminated.swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                // Stop new work while draining active files and finalization hooks.
                eprintln!("stopping after {skipped} consecutive skipped files");
            }
        }
    }
    for command in &args.exec_after {
        if let Err(e) = gallery_dl_rs::exec::run(command, None).await {
            eprintln!("postprocessing failed: {e}");
            failed.set(failed.get() + 1);
        }
    }
    if let Some(path) = &args.error_file {
        let lines: String = {
            let sources = input_sources.borrow();
            failed_inputs
                .borrow()
                .iter()
                .filter_map(|index| sources.get(index))
                .map(|source| format!("{source}\n"))
                .collect()
        };
        if !lines.is_empty() {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(|_| error("cannot open error file"))?;
            file.write_all(lines.as_bytes())
                .await
                .map_err(|_| error("cannot write error file"))?;
        }
    }
    let failed = failed.get();
    if failed > 0 {
        Err(error(format!("{failed} operation(s) failed")))
    } else {
        Ok(())
    }
}

/// Print one keyword tree like upstream `KeywordJob.print_kwdict`.
fn print_keywords(
    out: &mut String,
    keywords: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
) {
    use serde_json::Value;
    let suffix = if prefix.is_empty() { "" } else { "']" };
    let mut keys: Vec<_> = keywords.keys().collect();
    keys.sort();
    for key in keys {
        if key.starts_with('_') || prefix.is_empty() && key.starts_with("exts_") {
            continue;
        }
        let value = &keywords[key];
        let name = format!("{prefix}{key}{suffix}");
        match value {
            Value::Object(map) => print_keywords(out, map, &format!("{name}['")),
            Value::Array(items) if items.is_empty() => {}
            Value::Array(items) => {
                if let Some(Value::Object(first)) = items.first() {
                    print_keywords(out, first, &format!("{name}[N]['"));
                } else {
                    out.push_str(&format!("{name}[N]\n"));
                    let width = items.len().to_string().len();
                    for (index, item) in items.iter().enumerate() {
                        out.push_str(&format!("  {index:>width$} {}\n", python_text(item)));
                    }
                }
            }
            value => out.push_str(&format!("{name}\n  {}\n", python_text(value))),
        }
    }
}

fn python_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "None".into(),
        serde_json::Value::Bool(true) => "True".into(),
        serde_json::Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

/// `--list-extractors` / `--list-modules`.
fn list_extractors(detailed: bool) {
    let dedicated = [
        "instagram",
        "twitter",
        "facebook",
        "imgur",
        "bluesky",
        "reddit",
        "danbooru",
        "e621",
        "gelbooru",
        "safebooru",
        "mastodon.social",
        "pawoo",
        "baraag",
        "kemono",
        "coomer",
        "pixeldrain",
        "catbox",
        "imgbox",
        "postimg",
        "tumblr",
        "pixiv",
        "pixiv-novel",
        "deviantart",
        "flickr",
        "wallhaven",
        "artstation",
        "pinterest",
        "newgrounds",
        "furaffinity",
        "weasyl",
        "vsco",
    ];
    let mut lines: Vec<String> = dedicated.iter().map(|c| (*c).to_owned()).collect();
    for site in gallery_dl_rs::sites::sites() {
        if detailed {
            // Per-URL subcategories (generic, directlink) have no fixed name.
            let routes: Vec<_> = site
                .routes
                .iter()
                .map(|route| route.subcategory)
                .filter(|subcategory| !subcategory.is_empty())
                .collect();
            lines.push(if routes.is_empty() {
                site.category.to_owned()
            } else {
                format!("{}: {}", site.category, routes.join(", "))
            });
        } else {
            lines.push(site.category.to_owned());
        }
    }
    lines.sort();
    for line in lines {
        println!("{line}");
    }
}

#[derive(Default)]
struct Extractors {
    instagram: OnceCell<Result<Instagram>>,
    twitter: OnceCell<Result<Twitter>>,
    facebook: OnceCell<Result<Facebook>>,
    imgur: OnceCell<Result<gallery_dl_rs::imgur::Imgur>>,
    bluesky: OnceCell<Result<gallery_dl_rs::bluesky::Bluesky>>,
    reddit: OnceCell<Result<gallery_dl_rs::reddit::Reddit>>,
    booru: [OnceCell<Result<gallery_dl_rs::booru::Booru>>; 4],
    booru_artist: [OnceCell<Result<gallery_dl_rs::booru::Booru>>; 4],
    booru_artist_search: [OnceCell<Result<gallery_dl_rs::booru::Booru>>; 4],
    mastodon: [OnceCell<Result<gallery_dl_rs::mastodon::Mastodon>>; 3],
    kemono: [OnceCell<Result<gallery_dl_rs::kemono::Kemono>>; 2],
    imagehosts: [OnceCell<Result<gallery_dl_rs::imagehosts::ImageHosts>>; 4],
    tumblr: OnceCell<Result<gallery_dl_rs::tumblr::Tumblr>>,
    pixiv: OnceCell<Result<gallery_dl_rs::pixiv::Pixiv>>,
    pixiv_novel: OnceCell<Result<Arc<gallery_dl_rs::pixiv::Pixiv>>>,
    pixiv_artwork: OnceCell<Result<Arc<gallery_dl_rs::pixiv::Pixiv>>>,
    deviantart: OnceCell<Result<gallery_dl_rs::deviantart::DeviantArt>>,
    flickr: OnceCell<Result<gallery_dl_rs::flickr::Flickr>>,
    wallhaven: OnceCell<Result<gallery_dl_rs::wallhaven::Wallhaven>>,
    artstation: OnceCell<Result<gallery_dl_rs::artstation::ArtStation>>,
    pinterest: OnceCell<Result<gallery_dl_rs::pinterest::Pinterest>>,
    newgrounds: OnceCell<Result<gallery_dl_rs::newgrounds::Newgrounds>>,
    furaffinity: OnceCell<Result<gallery_dl_rs::furaffinity::FurAffinity>>,
    weasyl: OnceCell<Result<gallery_dl_rs::weasyl::Weasyl>>,
    vsco: OnceCell<Result<gallery_dl_rs::vsco::Vsco>>,
    sites: gallery_dl_rs::sites::Runtime,
    /// yt-dlp executable for sites extracted through it.
    ytdl: PathBuf,
    /// Session cookies forwarded to yt-dlp.
    ytdl_cookies: Option<gallery_dl_rs::cookies::YtdlCookies>,
}

/// The command-line session cookies, forwarded to yt-dlp as given.
fn ytdl_cookies(args: &Args) -> Option<gallery_dl_rs::cookies::YtdlCookies> {
    use gallery_dl_rs::cookies::YtdlCookies;
    args.cookies
        .clone()
        .map(YtdlCookies::File)
        .or_else(|| args.cookies_from_browser.clone().map(YtdlCookies::Browser))
}

impl Extractors {
    async fn cookie_jar(&self, settings: &Settings, site: &str) -> Result<CookieJar> {
        if let Some(browser) = &settings.browser {
            return browser.load(site).await;
        }
        match &settings.options.cookies {
            Some(path) => CookieJar::parse(&tokio::fs::read_to_string(path).await?),
            None => Ok(CookieJar::default()),
        }
    }
}

/// Child jobs for URLs queued by a native extractor to another site, routed
/// exactly like command-line inputs. Unroutable URLs are skipped.
fn child_queue<'a>(
    extractors: &'a Extractors,
    backend: &'a Backend,
    extractor: Extractor,
    settings: &'a BTreeMap<&str, Settings>,
) -> event::Queue<'a> {
    Arc::new(move |url: url::Url| {
        routable(&url, extractor, settings)
            .then(|| extract_input(url.to_string(), extractors, backend, extractor, settings))
    })
}

/// Whether a child URL can be routed like a command-line input.
fn routable(url: &url::Url, extractor: Extractor, settings: &BTreeMap<&str, Settings>) -> bool {
    routed_site(url, extractor).is_some()
        || extract::direct(url).is_some()
        || matches!(extractor, Extractor::Compatibility)
        || generic_fallback(settings)
}

/// Upstream's `extractor.generic.enabled`: URLs no other extractor matches
/// go to the generic extractor without its `generic:` prefix.
fn generic_fallback(settings: &BTreeMap<&str, Settings>) -> bool {
    settings
        .get("generic")
        .and_then(|generic| generic.options.site_options.as_ref())
        .and_then(|options| options.get("enabled"))
        .is_some_and(|enabled| match enabled {
            serde_json::Value::Bool(value) => *value,
            serde_json::Value::Number(value) => value.as_f64() != Some(0.0),
            serde_json::Value::String(value) => !value.is_empty(),
            serde_json::Value::Array(value) => !value.is_empty(),
            serde_json::Value::Object(value) => !value.is_empty(),
            serde_json::Value::Null => false,
        })
}

fn extract_input<'a>(
    source: String,
    extractors: &'a Extractors,
    backend: &'a Backend,
    extractor: Extractor,
    settings: &'a BTreeMap<&str, Settings>,
) -> BoxStream<'a, Result<ExtractionEvent>> {
    let input = stream::once(async move {
        let url = extract::input_url(&source)?;
        let input: BoxStream<'a, Result<ExtractionEvent>> = if let Some(category) =
            routed_site(&url, extractor)
        {
            let configured = &settings[settings_key(&url, extractor)];
            let options = &configured.options;
            match extractor {
                Extractor::Native if category == "instagram" => {
                    let native = extractors
                        .instagram
                        .get_or_init(|| async {
                            Instagram::from_cookie_jar(
                                extractors.cookie_jar(configured, "instagram.com").await?,
                                backend.timeout,
                            )
                            .and_then(|native| {
                                native
                                    .with_include(options.include.clone())
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Instagram initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if category == "twitter" => {
                    let native = extractors
                        .twitter
                        .get_or_init(|| async {
                            Twitter::from_cookie_jar(
                                extractors.cookie_jar(configured, "x.com").await?,
                                backend.timeout,
                            )
                            .and_then(|native| {
                                native
                                    .with_include(options.include.clone())
                                    .with_expansion(ExpansionOptions {
                                        retweets: options.retweets.unwrap_or_default(),
                                        quoted: options.quoted.unwrap_or(false),
                                    })
                                    .with_text_tweets(options.text_tweets.unwrap_or(false))
                                    .with_image_options(options.size.clone().unwrap_or_default())
                                    .with_article_options(options.articles.unwrap_or_default())
                                    .with_card_options(CardOptions {
                                        mode: options.cards.unwrap_or_default(),
                                        blacklist: options
                                            .cards_blacklist
                                            .clone()
                                            .unwrap_or_default(),
                                    })
                                    .with_media_options(MediaOptions {
                                        videos: options.videos.unwrap_or_default(),
                                        previews: options.previews.unwrap_or(false),
                                        unavailable: options.unavailable.unwrap_or(false),
                                    })
                                    .with_thread_options(ThreadOptions {
                                        expand: options.expand.unwrap_or(false),
                                        unique: options.unique.unwrap_or(true),
                                    })
                                    .with_status_options(StatusOptions {
                                        endpoint: options.tweet_endpoint.unwrap_or_default(),
                                        conversations: options.conversations.unwrap_or_default(),
                                    })
                                    .with_timeline_options(TimelineOptions {
                                        limit: options.limit.unwrap_or(50),
                                        strategy: options.strategy.unwrap_or_default(),
                                        search_pagination: options
                                            .search_pagination
                                            .unwrap_or_default(),
                                        search_product: options.search_results.unwrap_or_default(),
                                        search_limit: options.search_limit.unwrap_or(20),
                                        search_stop: options.search_stop.unwrap_or(3),
                                    })
                                    .with_profile_options(ProfileOptions {
                                        pinned: options.pinned.unwrap_or_default(),
                                        replies: options.replies.unwrap_or_default(),
                                    })
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Twitter initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "facebook" => {
                    let native = extractors
                        .facebook
                        .get_or_init(|| async {
                            Facebook::from_cookie_jar(
                                extractors.cookie_jar(configured, "facebook.com").await?,
                                backend.timeout,
                            )
                            .and_then(|native| {
                                native
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Facebook initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if category == "imgur" => {
                    let native = extractors
                        .imgur
                        .get_or_init(|| async {
                            gallery_dl_rs::imgur::Imgur::new(backend.timeout).and_then(|native| {
                                native
                                    .with_mp4(options.mp4.unwrap_or(true))
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Imgur initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "bluesky" => {
                    let native = extractors
                        .bluesky
                        .get_or_init(|| async {
                            gallery_dl_rs::bluesky::Bluesky::new(backend.timeout).and_then(
                                |native| {
                                    if options.videos
                                        == Some(gallery_dl_rs::twitter::VideoMode::Ytdl)
                                    {
                                        return Err(error(
                                            "native Bluesky videos must be a boolean",
                                        ));
                                    }
                                    native
                                        .with_api_server(options.api_server.as_deref())?
                                        .with_likes_endpoint(
                                            options.bluesky_endpoint.unwrap_or_default(),
                                        )
                                        .with_include(options.include.clone())
                                        .with_selection(gallery_dl_rs::bluesky::Selection {
                                            reposts: options.reposts.unwrap_or(false),
                                            videos: options.videos
                                                != Some(gallery_dl_rs::twitter::VideoMode::Skip),
                                            quoted: options.quoted.unwrap_or(false),
                                        })
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Bluesky initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "reddit" => {
                    let native = extractors
                        .reddit
                        .get_or_init(|| async {
                            let jar = extractors.cookie_jar(configured, "reddit.com").await?;
                            gallery_dl_rs::reddit::Reddit::new(backend.timeout).and_then(|native| {
                                native
                                    .with_cookies(jar)
                                    .with_options(options.site_options.as_deref())?
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Reddit initialization failed: {e}")))?;
                    native.events_with_queue(
                        &url,
                        child_queue(extractors, backend, extractor, settings),
                    )
                }
                Extractor::Native if category == "mastodon" => {
                    let host = url.host_str().expect("Mastodon input host");
                    let slot = gallery_dl_rs::mastodon::INSTANCES
                        .iter()
                        .position(|candidate| *candidate == host)
                        .expect("supported instance");
                    let native = extractors.mastodon[slot]
                        .get_or_init(|| async {
                            gallery_dl_rs::mastodon::Mastodon::new(backend.timeout).and_then(
                                |native| {
                                    if options.replies
                                        == Some(gallery_dl_rs::twitter::ReplyMode::SelfOnly)
                                    {
                                        return Err(error(
                                            "native Mastodon replies must be a boolean",
                                        ));
                                    }
                                    if options.cards == Some(gallery_dl_rs::twitter::CardMode::Ytdl)
                                    {
                                        return Err(error(
                                            "native Mastodon cards must be a boolean",
                                        ));
                                    }
                                    native
                                        .with_access_token_for(
                                            host,
                                            options.access_token.as_deref(),
                                        )?
                                        .with_selection(gallery_dl_rs::mastodon::SelectionOptions {
                                            reblogs: options.reblogs.unwrap_or(false),
                                            replies: options.replies
                                                != Some(gallery_dl_rs::twitter::ReplyMode::Skip),
                                            text_posts: options.text_posts.unwrap_or(false),
                                            cards: options.cards
                                                == Some(gallery_dl_rs::twitter::CardMode::Files),
                                        })
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Mastodon initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if matches!(category, "kemono" | "coomer") => {
                    let native = extractors.kemono[usize::from(category == "coomer")]
                        .get_or_init(|| async {
                            let site = if category == "coomer" {
                                "coomer.st"
                            } else {
                                "kemono.cr"
                            };
                            let jar = extractors.cookie_jar(configured, site).await?;
                            gallery_dl_rs::kemono::Kemono::new(backend.timeout).and_then(|native| {
                                let native = native.with_cookies(jar);
                                native
                                    .with_expand(options.expand.unwrap_or(false))
                                    .with_selection(options.kemono_selection())
                                    .with_revisions(options.revisions.unwrap_or_default())
                                    .with_revision_order(
                                        options.order_revisions.unwrap_or_default(),
                                    )
                                    .with_endpoint(options.endpoint.unwrap_or_default())
                                    .with_post_order(options.order_posts.unwrap_or_default())
                                    .with_favorites(options.favorites.unwrap_or_default())
                                    .with_max_posts(options.max_posts)
                                    .with_metadata(options.metadata.unwrap_or(true))
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Kemono initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "tumblr" => {
                    let native = extractors
                        .tumblr
                        .get_or_init(|| async {
                            gallery_dl_rs::tumblr::Tumblr::new(backend.timeout).and_then(|native| {
                                native
                                    .with_selection(options.tumblr_selection())
                                    .with_options(options.site_options.as_deref())?
                                    .with_api_key(options.api_key.as_deref())
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Tumblr initialization failed: {e}")))?;
                    native.events_with_queue(
                        &url,
                        child_queue(extractors, backend, extractor, settings),
                    )
                }
                Extractor::Native if category == "flickr" => {
                    let native = extractors
                        .flickr
                        .get_or_init(|| async {
                            gallery_dl_rs::flickr::Flickr::new(backend.timeout).and_then(|native| {
                                native
                                    .with_options(options.site_options.as_deref())?
                                    .with_api_key(options.api_key.as_deref())
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Flickr initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "artstation" => {
                    let native = extractors
                        .artstation
                        .get_or_init(|| async {
                            gallery_dl_rs::artstation::ArtStation::new(backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native ArtStation initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if category == "vsco" => {
                    let native = extractors
                        .vsco
                        .get_or_init(|| async {
                            gallery_dl_rs::vsco::Vsco::new(backend.timeout).and_then(|native| {
                                native
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native VSCO initialization failed: {e}")))?;
                    native.events(&url)
                }
                _ if gallery_dl_rs::sites::is_category(category)
                    && (matches!(extractor, Extractor::Native)
                        || gallery_dl_rs::sites::native_only(category)) =>
                {
                    let site = gallery_dl_rs::sites::site(category).expect("registry site");
                    // Cookies are read once, when the site's state is created.
                    let jar = match site.cookies {
                        Some(domain) if !extractors.sites.initialized(site) => {
                            let mut jar = extractors.cookie_jar(configured, domain).await?;
                            for extra in gallery_dl_rs::sites::extra_cookie_domains(site) {
                                jar.extend(extractors.cookie_jar(configured, extra).await?);
                            }
                            Some(jar)
                        }
                        _ => None,
                    };
                    let native = extractors
                        .sites
                        .get(site, || {
                            gallery_dl_rs::sites::Http::new(site.name, backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_cookies(jar.unwrap_or_default())
                                        .with_ytdl(&extractors.ytdl)
                                        .with_ytdl_cookies(extractors.ytdl_cookies.clone())
                                        .with_api_key(options.api_key.as_deref())
                                        .with_options(options.site_options.clone())
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .map_err(|e| {
                            error(format!("native {} initialization failed: {e}", site.name))
                        })?;
                    (site.extract)(native, url.clone())
                }
                Extractor::Native if category == "weasyl" => {
                    let native = extractors
                        .weasyl
                        .get_or_init(|| async {
                            gallery_dl_rs::weasyl::Weasyl::new(backend.timeout).and_then(|native| {
                                native
                                    .with_api_key(options.api_key.as_deref())
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Weasyl initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "furaffinity" => {
                    let native = extractors
                        .furaffinity
                        .get_or_init(|| async {
                            gallery_dl_rs::furaffinity::FurAffinity::from_cookie_jar(
                                extractors.cookie_jar(configured, "furaffinity.net").await?,
                                backend.timeout,
                            )
                            .and_then(|native| {
                                native
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Fur Affinity initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if category == "newgrounds" => {
                    let native = extractors
                        .newgrounds
                        .get_or_init(|| async {
                            gallery_dl_rs::newgrounds::Newgrounds::new(backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Newgrounds initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if category == "pinterest" => {
                    let native = extractors
                        .pinterest
                        .get_or_init(|| async {
                            gallery_dl_rs::pinterest::Pinterest::new(backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_options(options.site_options.as_deref())?
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Pinterest initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if category == "wallhaven" => {
                    let native = extractors
                        .wallhaven
                        .get_or_init(|| async {
                            gallery_dl_rs::wallhaven::Wallhaven::new(backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_api_key(options.api_key.as_deref())
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Wallhaven initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if category == "deviantart" => {
                    let native = extractors
                        .deviantart
                        .get_or_init(|| async {
                            gallery_dl_rs::deviantart::DeviantArt::new(backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_credentials(
                                            options.client_id.as_deref(),
                                            options.client_secret.as_deref(),
                                        )
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native DeviantArt initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native if matches!(category, "pixiv" | "pixiv-novel") => {
                    let novel_settings = &settings["pixiv-novel"];
                    let novel_options = &novel_settings.options;
                    let artwork = if novel_options.embeds.unwrap_or(false) {
                        let artwork_settings = &settings["pixiv"];
                        let artwork_options = &artwork_settings.options;
                        Some(
                            extractors
                                .pixiv_artwork
                                .get_or_init(|| async {
                                    let jar = extractors
                                        .cookie_jar(artwork_settings, "pixiv.net")
                                        .await?;
                                    gallery_dl_rs::pixiv::Pixiv::new(backend.timeout).and_then(
                                        |native| {
                                            native
                                                .with_cookies(jar)
                                                .with_captions(
                                                    artwork_options.captions.unwrap_or(false),
                                                )
                                                .with_sanity(artwork_options.sanity.unwrap_or(true))
                                                .with_ugoira_mode(
                                                    artwork_options.ugoira.unwrap_or_default(),
                                                )
                                                .with_metadata(
                                                    artwork_options.metadata.unwrap_or(false),
                                                )
                                                .with_comments(
                                                    artwork_options.comments.unwrap_or(false),
                                                )
                                                .with_metadata_bookmark(
                                                    artwork_options
                                                        .metadata_bookmark
                                                        .unwrap_or(false),
                                                )
                                                .with_max_posts(artwork_options.max_posts)
                                                .with_tags(artwork_options.tags.unwrap_or_default())
                                                .with_cursor(artwork_options.cursor.clone())
                                                .with_refresh_token(
                                                    artwork_options.refresh_token.as_deref(),
                                                )
                                                .with_retries(artwork_options.retries.unwrap())
                                                .with_request_timeout(
                                                    artwork_options.timeout.unwrap(),
                                                )
                                                .with_network(&artwork_settings.network)
                                                .map(Arc::new)
                                        },
                                    )
                                })
                                .await
                                .as_ref()
                                .map_err(|e| {
                                    error(format!(
                                        "native Pixiv artwork initialization failed: {e}"
                                    ))
                                })?,
                        )
                    } else {
                        None
                    };
                    let novel = extractors
                        .pixiv_novel
                        .get_or_init(|| async {
                            let jar = extractors.cookie_jar(novel_settings, "pixiv.net").await?;
                            gallery_dl_rs::pixiv::Pixiv::new(backend.timeout).and_then(|native| {
                                let native = native.with_cookies(jar);
                                let native = match artwork {
                                    Some(artwork) => {
                                        native.with_artwork_extractor(Arc::clone(artwork))
                                    }
                                    None => native,
                                };
                                native
                                    .with_novel_options(gallery_dl_rs::pixiv::NovelOptions {
                                        embeds: novel_options.embeds.unwrap_or(false),
                                        covers: novel_options.covers.unwrap_or(false),
                                        full_series: novel_options.full_series.unwrap_or(false),
                                    })
                                    .with_metadata(novel_options.metadata.unwrap_or(false))
                                    .with_comments(novel_options.comments.unwrap_or(false))
                                    .with_metadata_bookmark(
                                        novel_options.metadata_bookmark.unwrap_or(false),
                                    )
                                    .with_max_posts(novel_options.max_posts)
                                    .with_tags(novel_options.tags.unwrap_or_default())
                                    .with_cursor(novel_options.cursor.clone())
                                    .with_refresh_token(novel_options.refresh_token.as_deref())
                                    .with_retries(novel_options.retries.unwrap())
                                    .with_request_timeout(novel_options.timeout.unwrap())
                                    .with_network(&novel_settings.network)
                                    .map(Arc::new)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native Pixiv novel initialization failed: {e}"))
                        })?;
                    if category == "pixiv-novel" {
                        novel.events(&url)
                    } else {
                        let native = extractors
                            .pixiv
                            .get_or_init(|| async {
                                let jar = extractors.cookie_jar(configured, "pixiv.net").await?;
                                gallery_dl_rs::pixiv::Pixiv::new(backend.timeout).and_then(
                                    |native| {
                                        native
                                            .with_cookies(jar)
                                            .with_captions(options.captions.unwrap_or(false))
                                            .with_sanity(options.sanity.unwrap_or(true))
                                            .with_novel_extractor(Arc::clone(novel))
                                            .with_include(options.include.clone())
                                            .with_ugoira_mode(options.ugoira.unwrap_or_default())
                                            .with_metadata(options.metadata.unwrap_or(false))
                                            .with_comments(options.comments.unwrap_or(false))
                                            .with_metadata_bookmark(
                                                options.metadata_bookmark.unwrap_or(false),
                                            )
                                            .with_max_posts(options.max_posts)
                                            .with_tags(options.tags.unwrap_or_default())
                                            .with_cursor(options.cursor.clone())
                                            .with_refresh_token(options.refresh_token.as_deref())
                                            .with_retries(options.retries.unwrap())
                                            .with_request_timeout(options.timeout.unwrap())
                                            .with_network(&configured.network)
                                    },
                                )
                            })
                            .await
                            .as_ref()
                            .map_err(|e| {
                                error(format!("native Pixiv initialization failed: {e}"))
                            })?;
                        native.events(&url)
                    }
                }
                Extractor::Native
                    if let Some(site) =
                        gallery_dl_rs::imagehosts::Site::from_category(category) =>
                {
                    let native = extractors.imagehosts[site as usize]
                        .get_or_init(|| async {
                            gallery_dl_rs::imagehosts::ImageHosts::new(backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_pixeldrain_api_key(options.api_key.as_deref())?
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| {
                            error(format!("native image host initialization failed: {e}"))
                        })?;
                    native.events(&url)
                }
                Extractor::Native
                    if let Some(site) = gallery_dl_rs::booru::Site::from_category(category) =>
                {
                    let cache = match booru_subcategory(&url) {
                        Some("artist") => &extractors.booru_artist,
                        Some("artist-search") => &extractors.booru_artist_search,
                        _ => &extractors.booru,
                    };
                    let native = cache[site as usize]
                        .get_or_init(|| async {
                            gallery_dl_rs::booru::Booru::new(site, backend.timeout).and_then(
                                |native| {
                                    native
                                        .with_credentials(
                                            options.username.as_deref(),
                                            options.password.as_deref(),
                                        )?
                                        .with_api_credentials(
                                            options.user_id.as_deref(),
                                            options.api_key.as_deref(),
                                        )?
                                        .with_artist_parent_metadata(
                                            options.artist_parent_metadata().cloned(),
                                        )
                                        .with_threshold(options.booru_threshold.flatten())
                                        .with_favorite_order(options.booru_favorite_order.clone())
                                        .with_pool_order(
                                            options.booru_order_posts.unwrap_or_default(),
                                        )
                                        .with_retries(options.retries.unwrap())
                                        .with_request_timeout(options.timeout.unwrap())
                                        .with_network(&configured.network)
                                },
                            )
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native booru initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native => {
                    return Err(error(
                        "native extractor unavailable; use --extractor compatibility",
                    ));
                }
                Extractor::Compatibility => {
                    let mut backend = backend.clone();
                    backend.cookies = options.cookies.clone();
                    backend.events_with_options(
                        &url,
                        BackendOptions {
                            include: options.include.clone(),
                            child_range: options.child_range.clone(),
                            child_filter: options.child_filter.clone(),
                            browser: configured.browser_spec.clone(),
                            proxy: configured.network.proxy().map(str::to_owned),
                            user_agent: configured.network.user_agent().map(str::to_owned),
                            retweets: (category == "twitter")
                                .then_some(options.retweets)
                                .flatten(),
                            quoted: matches!(category, "twitter" | "bluesky")
                                .then_some(options.quoted)
                                .flatten(),
                            expand: matches!(category, "twitter" | "kemono" | "coomer")
                                .then_some(options.expand)
                                .flatten(),
                            parent_metadata: options.parent_metadata.as_deref().cloned(),
                            metadata_parent: options.metadata_parent.as_deref().cloned(),
                            booru_threshold: matches!(category, "danbooru" | "e621")
                                .then_some(options.booru_threshold)
                                .flatten(),
                            max_posts: matches!(
                                category,
                                "kemono" | "coomer" | "pixiv" | "pixiv-novel"
                            )
                            .then_some(options.max_posts)
                            .flatten(),
                            endpoint: matches!(category, "kemono" | "coomer")
                                .then_some(options.endpoint)
                                .flatten(),
                            bluesky_endpoint: (category == "bluesky")
                                .then_some(options.bluesky_endpoint)
                                .flatten(),
                            api_server: (category == "bluesky")
                                .then(|| options.api_server.clone())
                                .flatten(),
                            favorites: matches!(category, "kemono" | "coomer")
                                .then_some(options.favorites)
                                .flatten(),
                            booru_favorite_order: (category == "gelbooru")
                                .then(|| options.booru_favorite_order.clone())
                                .flatten(),
                            booru_order_posts: (category == "danbooru")
                                .then_some(options.booru_order_posts)
                                .flatten(),
                            order_posts: matches!(category, "kemono" | "coomer")
                                .then_some(options.order_posts)
                                .flatten(),
                            metadata: matches!(
                                category,
                                "kemono" | "coomer" | "pixiv" | "pixiv-novel"
                            )
                            .then_some(options.metadata)
                            .flatten(),
                            unique: (category == "twitter").then_some(options.unique).flatten(),
                            text_tweets: (category == "twitter")
                                .then_some(options.text_tweets)
                                .flatten(),
                            unavailable: (category == "twitter")
                                .then_some(options.unavailable)
                                .flatten(),
                            size: (category == "twitter")
                                .then(|| options.size.clone())
                                .flatten(),
                            mp4: (category == "imgur").then_some(options.mp4).flatten(),
                            api_key: options.api_key.clone(),
                            username: options.username.clone(),
                            password: options.password.clone(),
                            user_id: options.user_id.clone(),
                            tumblr_reblogs: (category == "tumblr"
                                && (options.tumblr_reblogs.is_some() || options.reblogs.is_some()))
                            .then(|| options.tumblr_selection().reblogs),
                            posts: (category == "tumblr")
                                .then(|| options.posts.clone())
                                .flatten(),
                            inline: (category == "tumblr").then_some(options.inline).flatten(),
                            original: matches!(category, "tumblr" | "kemono" | "coomer")
                                .then_some(options.original)
                                .flatten(),
                            files: matches!(category, "kemono" | "coomer")
                                .then(|| options.files.clone())
                                .flatten(),
                            duplicates: matches!(category, "kemono" | "coomer")
                                .then_some(options.duplicates)
                                .flatten(),
                            revisions: matches!(category, "kemono" | "coomer")
                                .then_some(options.revisions)
                                .flatten(),
                            order_revisions: matches!(category, "kemono" | "coomer")
                                .then_some(options.order_revisions)
                                .flatten(),
                            reposts: (category == "bluesky").then_some(options.reposts).flatten(),
                            sanity: (category == "pixiv").then_some(options.sanity).flatten(),
                            captions: (category == "pixiv").then_some(options.captions).flatten(),
                            comments: matches!(category, "pixiv" | "pixiv-novel")
                                .then_some(options.comments)
                                .flatten(),
                            metadata_bookmark: matches!(category, "pixiv" | "pixiv-novel")
                                .then_some(options.metadata_bookmark)
                                .flatten(),
                            ugoira: (category == "pixiv").then_some(options.ugoira).flatten(),
                            tags: matches!(category, "pixiv" | "pixiv-novel")
                                .then_some(options.tags)
                                .flatten(),
                            cursor: matches!(category, "pixiv" | "pixiv-novel")
                                .then(|| options.cursor.clone())
                                .flatten(),
                            embeds: (category == "pixiv-novel")
                                .then_some(options.embeds)
                                .flatten(),
                            covers: (category == "pixiv-novel")
                                .then_some(options.covers)
                                .flatten(),
                            full_series: (category == "pixiv-novel")
                                .then_some(options.full_series)
                                .flatten(),
                            access_token: (category == "mastodon")
                                .then(|| options.access_token.clone())
                                .flatten(),
                            reblogs: (category == "mastodon")
                                .then_some(options.reblogs)
                                .flatten(),
                            text_posts: (category == "mastodon")
                                .then_some(options.text_posts)
                                .flatten(),
                            fallback: options.fallback,
                            articles: (category == "twitter")
                                .then_some(options.articles)
                                .flatten(),
                            cards: matches!(category, "twitter" | "mastodon")
                                .then_some(options.cards)
                                .flatten(),
                            cards_blacklist: (category == "twitter")
                                .then(|| options.cards_blacklist.clone())
                                .flatten(),
                            videos: matches!(category, "twitter" | "bluesky")
                                .then_some(options.videos)
                                .flatten(),
                            previews: (category == "twitter")
                                .then_some(options.previews)
                                .flatten(),
                            tweet_endpoint: (category == "twitter")
                                .then_some(options.tweet_endpoint)
                                .flatten(),
                            conversations: (category == "twitter")
                                .then_some(options.conversations)
                                .flatten(),
                            pinned: (category == "twitter").then_some(options.pinned).flatten(),
                            replies: matches!(category, "twitter" | "mastodon")
                                .then_some(options.replies)
                                .flatten(),
                            strategy: (category == "twitter")
                                .then_some(options.strategy)
                                .flatten(),
                            search_pagination: (category == "twitter")
                                .then_some(options.search_pagination)
                                .flatten(),
                            search_results: (category == "twitter")
                                .then_some(options.search_results)
                                .flatten(),
                            limit: (category == "twitter").then_some(options.limit).flatten(),
                            search_limit: (category == "twitter")
                                .then_some(options.search_limit)
                                .flatten(),
                            search_stop: (category == "twitter")
                                .then_some(options.search_stop)
                                .flatten(),
                            options: configured.cli_options.clone(),
                        },
                    )
                }
            }
        } else if let Some(item) = extract::direct(&url) {
            stream::iter(event::group(vec![item], "direct").into_iter().map(Ok)).boxed()
        } else if matches!(extractor, Extractor::Compatibility) {
            // Any other gallery-dl extractor; files keep its category.
            let configured = &settings["direct"];
            let mut backend = backend.clone();
            backend.cookies = configured.options.cookies.clone();
            backend.events_with_options(
                &url,
                BackendOptions {
                    browser: configured.browser_spec.clone(),
                    proxy: configured.network.proxy().map(str::to_owned),
                    user_agent: configured.network.user_agent().map(str::to_owned),
                    options: configured.cli_options.clone(),
                    ..BackendOptions::default()
                },
            )
        } else if generic_fallback(settings) {
            // Upstream's generic `enabled` option makes its prefix optional.
            extract_input(format!("generic:{url}"), extractors, backend, extractor, settings)
        } else {
            return Err(error(
                "unsupported URL; expected a social URL or direct media file (other gallery-dl sites need --extractor compatibility)",
            ));
        };
        let input = if routed_site(&url, extractor) == Some("mastodon") {
            let category = settings_category(&url, extractor);
            input
                .map(move |event| {
                    event.map(|mut event| {
                        match &mut event {
                            ExtractionEvent::Media(media) => media.category = category.into(),
                            ExtractionEvent::Post(post) => post.category = category.into(),
                            ExtractionEvent::EndPost
                            | ExtractionEvent::BeginChild(_)
                            | ExtractionEvent::EndChild(_) => {}
                        }
                        event
                    })
                })
                .boxed()
        } else {
            input
        };
        Ok(input)
    })
    .try_flatten()
    .boxed();
    event::with_extraction_budget(input, backend.timeout)
}

struct Settings {
    options: ConfigOptions,
    /// Raw `-o extractor.*` settings for the compatibility backend.
    cli_options: Vec<String>,
    browser: Option<FirefoxProfile>,
    browser_spec: Option<String>,
    filename: Option<FilenameTemplate>,
    directory: Option<DirectoryTemplate>,
    selection: Option<RangeSelection>,
    filter: Option<FilterExpression>,
    post_selection: Option<PostSelection>,
    child_range: Option<RangeSelection>,
    child_filter: Option<FilterExpression>,
    network: NetworkOptions,
    postprocessors: Option<std::sync::Arc<gallery_dl_rs::postprocess::Postprocessors>>,
    download_client: OnceCell<Result<download::DownloadClient>>,
    limits: SizeLimits,
}

impl Settings {
    fn job_selection(&self, unordered: bool) -> JobSelection {
        JobSelection {
            post_selection: self.post_selection.clone(),
            file_range: self.selection.clone(),
            file_filter: self.filter.clone(),
            child_range: self.child_range.clone(),
            child_filter: self.child_filter.clone(),
            before: self.options.date_before,
            after: self.options.date_after,
            unordered,
        }
    }
}

async fn resolved_settings(args: &Args) -> Result<BTreeMap<&'static str, Settings>> {
    let discovered = if args.native_config.is_none() && !args.native_config_ignore {
        default_native_config()
    } else {
        None
    };
    let mut options = args
        .options
        .iter()
        .map(|option| gallery_dl_rs::config::parse_option(option))
        .collect::<Result<Vec<_>>>()?;
    // Upstream CLI postprocessor shortcuts.
    use gallery_dl_rs::config::CliOption;
    for name in &args.postprocessor {
        options.push(CliOption::postprocessor(serde_json::json!(name)));
    }
    if args.write_info_json {
        options.push(CliOption::postprocessor(serde_json::json!({
            "name": "metadata",
            "event": "init",
            "filename": "info.json",
        })));
    }
    if args.write_tags {
        options.push(CliOption::postprocessor(
            serde_json::json!({"name": "metadata", "mode": "tags"}),
        ));
    }
    let cli_options: Vec<String> = options
        .iter()
        .filter(|option| option.path.first().is_some_and(|part| part == "extractor"))
        .map(|option| option.raw.clone())
        .collect();
    let config = match args.native_config.as_ref().or(discovered.as_ref()) {
        Some(path) => Config::load_with_options(path, &options).await?,
        None if options.is_empty() => Config::default(),
        None => Config::parse_with_options(b"{}", &std::env::current_dir()?, &options)?,
    };
    let defaults = ConfigOptions {
        base_directory: Some(PathBuf::from("downloads")),
        retries: Some(2),
        timeout: Some(Duration::from_secs(60)),
        ..ConfigOptions::default()
    };
    let overrides = ConfigOptions {
        init: None,
        postprocessors: None,
        base_directory: args
            .exact_directory
            .clone()
            .or_else(|| args.destination.clone()),
        filename: args.filename.clone(),
        filename_conditions: None,
        file_range: args.file_range.clone(),
        file_filter: (!args.file_filter.is_empty()).then(|| args.file_filter.clone()),
        post_range: args.post_range.clone(),
        post_filter: (!args.post_filter.is_empty()).then(|| args.post_filter.clone()),
        child_range: args.child_range.clone(),
        child_filter: (!args.child_filter.is_empty()).then(|| args.child_filter.clone()),
        include: None,
        date_before: args
            .date_before
            .as_ref()
            .and_then(|value| source_date_bound(&serde_json::Value::String(value.clone()))),
        date_after: args
            .date_after
            .as_ref()
            .and_then(|value| source_date_bound(&serde_json::Value::String(value.clone()))),
        date_before_set: args.date_before.is_some(),
        date_after_set: args.date_after.is_some(),
        directory: if args.exact_directory.is_some() {
            Some(Vec::new())
        } else {
            (!args.directory.is_empty()).then(|| args.directory.clone())
        },
        directory_conditions: None,
        cookies: args.cookies.clone(),
        browser_cookies: None,
        retweets: None,
        quoted: None,
        expand: None,
        unique: None,
        videos: None,
        previews: None,
        mp4: None,
        reblogs: None,
        tumblr_reblogs: None,
        posts: None,
        inline: None,
        original: None,
        files: None,
        duplicates: None,
        revisions: None,
        order_revisions: None,
        endpoint: None,
        bluesky_endpoint: None,
        api_server: None,
        order_posts: None,
        booru_order_posts: None,
        booru_favorite_order: None,
        favorites: None,
        username: None,
        password: None,
        user_id: None,
        reposts: None,
        sanity: None,
        captions: None,
        comments: None,
        metadata_bookmark: None,
        ugoira: None,
        tags: None,
        cursor: None,
        access_token: None,
        text_posts: None,
        unavailable: None,
        text_tweets: if args.text_tweets {
            Some(true)
        } else if args.no_text_tweets {
            Some(false)
        } else {
            None
        },
        size: None,
        fallback: None,
        articles: None,
        cards: None,
        cards_blacklist: None,
        tweet_endpoint: None,
        conversations: None,
        pinned: None,
        replies: None,
        strategy: None,
        search_pagination: None,
        search_results: None,
        limit: None,
        search_limit: None,
        search_stop: None,
        max_posts: None,
        parent_metadata: None,
        metadata_parent: None,
        booru_threshold: None,
        metadata: None,
        api_key: None,
        refresh_token: None,
        embeds: None,
        covers: None,
        full_series: None,
        client_id: None,
        client_secret: None,
        proxy: args.proxy.clone(),
        user_agent: args.user_agent.clone(),
        archive: args.download_archive.clone(),
        filesize_min: args.filesize_min,
        filesize_max: args.filesize_max,
        retries: args.retries,
        timeout: args.timeout,
        site_options: None,
    };
    let mut settings = BTreeMap::new();
    for category in [
        "instagram",
        "twitter",
        "facebook",
        "imgur",
        "bluesky",
        "reddit",
        "danbooru",
        "e621",
        "gelbooru",
        "safebooru",
        "mastodon",
        "mastodon.social",
        "pawoo",
        "baraag",
        "kemono",
        "coomer",
        "pixeldrain",
        "catbox",
        "imgbox",
        "postimg",
        "tumblr",
        "pixiv",
        "pixiv-novel",
        "deviantart",
        "flickr",
        "wallhaven",
        "artstation",
        "pinterest",
        "newgrounds",
        "furaffinity",
        "weasyl",
        "vsco",
        "direct",
        "danbooru.artist",
        "danbooru.artist-search",
        "danbooru.tag",
        "e621.artist",
        "e621.artist-search",
        "e621.tag",
    ]
    .into_iter()
    .chain(gallery_dl_rs::sites::categories())
    {
        let class = category
            .split_once('.')
            .filter(|(base, _)| matches!(*base, "danbooru" | "e621"));
        let resolved = match class {
            Some((base, class)) => config.resolve_class(base, class)?,
            None => config.resolve(category)?,
        };
        let options = defaults.overlay(&resolved).overlay(&overrides);
        let browser_spec = args.cookies_from_browser.clone().or_else(|| {
            options.browser_cookies.as_ref().map(|profile| {
                // Config parsing has already checked that this path is UTF-8.
                format!("firefox:{}", profile.display())
            })
        });
        let browser = if matches!(args.extractor, Extractor::Native) {
            browser_spec
                .as_deref()
                .map(FirefoxProfile::parse)
                .transpose()?
        } else {
            None
        };
        let filename = match &options.filename_conditions {
            Some(conditions) => Some(FilenameTemplate::parse_source_conditional(
                conditions,
                options.filename.as_deref(),
            )?),
            None => options
                .filename
                .as_deref()
                .map(|template| FilenameTemplate::parse_source(template))
                .transpose()?,
        };
        let directory = match &options.directory_conditions {
            Some(conditions) => Some(DirectoryTemplate::parse_source_conditional(
                conditions,
                options.directory.as_deref(),
            )?),
            None => options
                .directory
                .as_deref()
                .map(|components| DirectoryTemplate::parse_source(components))
                .transpose()?,
        };
        let selection = options
            .file_range
            .as_deref()
            .map(RangeSelection::parse)
            .transpose()?;
        let filter = options
            .file_filter
            .as_deref()
            .map(FilterExpression::parse_all)
            .transpose()?;
        let post_range = options
            .post_range
            .as_deref()
            .map(RangeSelection::parse)
            .transpose()
            .map_err(|_| error("invalid post range"))?;
        let post_filter = options
            .post_filter
            .as_deref()
            .filter(|clauses| !clauses.is_empty())
            .map(FilterExpression::parse_all)
            .transpose()?;
        let post_selection = (post_range.is_some() || post_filter.is_some())
            .then(|| PostSelection::new(post_range, post_filter));
        let network = NetworkOptions::new(options.proxy.clone(), options.user_agent.clone())?;
        let limits = SizeLimits::new(options.filesize_min, options.filesize_max)?;
        let child_range = options
            .child_range
            .as_deref()
            .map(RangeSelection::parse)
            .transpose()
            .map_err(|_| error("invalid child range"))?;
        let child_filter = options
            .child_filter
            .as_deref()
            .filter(|clauses| !clauses.is_empty())
            .map(FilterExpression::parse_all)
            .transpose()?;
        let postprocessors = options
            .postprocessors
            .as_ref()
            .map(|config| {
                config.specifications().map(|specs| {
                    std::sync::Arc::new(gallery_dl_rs::postprocess::Postprocessors::new(specs))
                })
            })
            .transpose()?;
        settings.insert(
            category,
            Settings {
                options,
                cli_options: cli_options.clone(),
                browser,
                browser_spec,
                filename,
                directory,
                selection,
                filter,
                post_selection,
                child_range,
                child_filter,
                network,
                postprocessors,
                download_client: OnceCell::new(),
                limits,
            },
        );
    }
    Ok(settings)
}
