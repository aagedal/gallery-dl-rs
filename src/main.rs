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
    date::{DateFilter, parse_date},
    download::{self, DownloadOptions, Outcome},
    error,
    event::{self, ExtractionEvent},
    extract::{self, Backend, BackendOptions},
    facebook::Facebook,
    filter::FilterExpression,
    input::Inputs,
    instagram::Instagram,
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
    /// Select files with a metadata expression; repeat to require all expressions
    #[arg(long = "filter", value_name = "EXPR", allow_hyphen_values = true)]
    file_filter: Vec<String>,
    /// Select one-based post positions per input
    #[arg(long = "post-range", allow_hyphen_values = true)]
    post_range: Option<String>,
    /// Select posts before file filtering; repeat to require all expressions
    #[arg(long = "post-filter", value_name = "EXPR", allow_hyphen_values = true)]
    post_filter: Vec<String>,
    /// Emit metadata for native X posts without media
    #[arg(long, conflicts_with = "no_text_tweets")]
    text_tweets: bool,
    /// Disable configured X text-post output
    #[arg(long, conflicts_with = "text_tweets")]
    no_text_tweets: bool,
    /// Process posts created before this UTC date or Unix timestamp (exclusive)
    #[arg(long, value_parser = parse_date_arg, allow_hyphen_values = true)]
    date_before: Option<i64>,
    /// Stop this input at a post on or before this UTC date or Unix timestamp
    #[arg(long, value_parser = parse_date_arg, allow_hyphen_values = true)]
    date_after: Option<i64>,
    /// Minimum media size in bytes or binary units, e.g. 500k or 2.5M
    #[arg(long, value_parser = parse_size)]
    filesize_min: Option<u64>,
    /// Maximum media size in bytes or binary units (0 disables this bound)
    #[arg(long, value_parser = parse_size)]
    filesize_max: Option<u64>,
    /// Print media URLs without downloading (still performs extraction)
    #[arg(short = 'g', long, conflicts_with = "dump_json")]
    get_urls: bool,
    /// Print extracted media as a JSON array without downloading
    #[arg(short = 'j', long)]
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

fn parse_date_arg(value: &str) -> std::result::Result<i64, String> {
    parse_date(value).map_err(|e| e.to_string())
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
    // Inputs stopped by -A/--abort; their remaining media are neither
    // extracted nor downloaded.
    let aborted = std::cell::RefCell::new(HashSet::<usize>::new());
    let mut seen = HashSet::new();
    // Pull one input at a time, retaining one parsed message and active downloads.
    // buffer_unordered below supplies backpressure when all download slots are full.
    let media = inputs
        .stream()
        .enumerate()
        .flat_map(|(index, source)| {
            let failed = &failed;
            let aborted = &aborted;
            let input = match source {
                Ok(source) => {
                    let unordered_dates = extract::http_url(&source).ok().is_some_and(|url| {
                        gallery_dl_rs::instagram::is_story_url(&url)
                            || gallery_dl_rs::instagram::is_unordered_collection_url(&url)
                            || gallery_dl_rs::twitter::is_profile_timeline_url(&url)
                            || gallery_dl_rs::twitter::is_search_url(&url)
                            || gallery_dl_rs::twitter::is_collection_url(&url)
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
                    let selected_settings = extract::http_url(&source).ok().map(|url| {
                        &settings[routed_site(&url, args.extractor).unwrap_or("direct")]
                    });
                    let input =
                        extract_input(source, &extractors, &backend, args.extractor, &settings);
                    match selected_settings {
                        Some(settings) => {
                            let input = if let Some(selection) = &settings.post_selection {
                                selection.apply(
                                    input,
                                    settings.options.date_before,
                                    settings.options.date_after,
                                    unordered_dates,
                                )
                            } else {
                                let input = event::media_only(input);
                                if settings.options.date_before.is_some()
                                    || settings.options.date_after.is_some()
                                {
                                    if unordered_dates {
                                        settings.dates.apply_unordered(input)
                                    } else {
                                        settings.dates.apply(input)
                                    }
                                } else {
                                    input
                                }
                            };
                            let input = match &settings.filter {
                                Some(filter) => filter.apply(input),
                                None => input,
                            };
                            match &settings.selection {
                                Some(selection) => selection.apply(input),
                                None => input,
                            }
                        }
                        None => event::media_only(input),
                    }
                }
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            };
            input
                .take_while(move |_| std::future::ready(!aborted.borrow().contains(&index)))
                .filter_map(move |result| {
                    std::future::ready(match result {
                        Ok(item) => Some((index, item)),
                        Err(e) => {
                            eprintln!("extraction failed for input {}: {e}", index + 1);
                            failed.set(failed.get() + 1);
                            None
                        }
                    })
                })
        })
        .map(|(index, mut item)| {
            settings[item.category.as_str()]
                .network
                .apply_media(&mut item);
            (index, item)
        })
        .filter(|(_, item)| {
            if item.category == "twitter" && settings["twitter"].options.unique == Some(false) {
                return std::future::ready(true);
            }
            let mut identity = item.identity();
            // Distinct social files can share a URL and request context.
            if matches!(
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
                    | "kemono"
                    | "coomer"
                    | "pixeldrain"
                    | "catbox"
                    | "imgbox"
                    | "postimg"
                    | "tumblr"
                    | "pixiv"
                    | "deviantart"
                    | "flickr"
                    | "wallhaven"
                    | "artstation"
                    | "pinterest"
                    | "newgrounds"
                    | "furaffinity"
                    | "weasyl"
                    | "vsco"
            ) && let Some(file_key) = item.archive_key()
            {
                identity = serde_json::json!([file_key, identity]).to_string();
            }
            // Retain only fixed-size digests, without URLs, headers, or manifests.
            let digest: [u8; 32] = Sha256::digest(identity.as_bytes()).into();
            std::future::ready(seen.insert(digest))
        });
    tokio::pin!(media);
    if args.dump_json {
        let media: Vec<_> = media.map(|(_, item)| item).collect().await;
        println!("{}", serde_json::to_string_pretty(&media)?);
    } else if args.get_urls {
        while let Some((_, item)) = media.next().await {
            println!("{}", item.url);
        }
    } else {
        let client = download::client(Duration::from_secs(60))?
            .with_resume(args.resume)
            .with_http_mtime(!args.no_mtime)
            .with_metadata_mtime(args.mtime.clone())
            .with_sleep(args.sleep)
            .with_rate_limit(args.limit_rate);
        let aborted = &aborted;
        let results = media
            .map(|(index, item)| {
                let settings = &settings[item.category.as_str()];
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
                    retries: settings.options.retries.unwrap(),
                    limits: settings.limits,
                    write_metadata: args.write_metadata,
                    directory: settings.directory.as_ref(),
                };
                async move {
                    // Downloads queued before an abort are dropped unstarted.
                    if aborted.borrow().contains(&index) {
                        return (index, None);
                    }
                    let result: Result<ArchiveOutcome> = async {
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
            if args.abort.is_some_and(|limit| *input >= limit) && aborted.borrow_mut().insert(index)
            {
                eprintln!(
                    "stopping input {} after {input} consecutive skipped files",
                    index + 1
                );
            }
            match result {
                Ok(ArchiveOutcome::Completed(Outcome::Downloaded(path))) => {
                    eprintln!("downloaded {}", path.display());
                    for command in &args.exec {
                        if let Err(e) = gallery_dl_rs::exec::run(command, Some(&path)).await {
                            eprintln!("postprocessing failed for {}: {e}", path.display());
                            failed.set(failed.get() + 1);
                        }
                    }
                }
                Ok(ArchiveOutcome::Completed(Outcome::Skipped(path))) => {
                    eprintln!("skipped {}", path.display())
                }
                Ok(ArchiveOutcome::Completed(Outcome::Filtered(reason))) => {
                    eprintln!("filtered media: {reason}")
                }
                Ok(ArchiveOutcome::AlreadyArchived) => eprintln!("skipped archived media"),
                Err(e) => {
                    eprintln!("download failed: {e}");
                    failed.set(failed.get() + 1);
                }
            }
            if args.terminate.is_some_and(|limit| skipped >= limit) {
                // Dropping the stream cancels pending extraction and downloads.
                eprintln!("stopping after {skipped} consecutive skipped files");
                break;
            }
        }
    }
    for command in &args.exec_after {
        if let Err(e) = gallery_dl_rs::exec::run(command, None).await {
            eprintln!("postprocessing failed: {e}");
            failed.set(failed.get() + 1);
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
    imgur: OnceCell<Result<gallery_dl_rs::imgur::Imgur>>,
    bluesky: OnceCell<Result<gallery_dl_rs::bluesky::Bluesky>>,
    reddit: OnceCell<Result<gallery_dl_rs::reddit::Reddit>>,
    booru: [OnceCell<Result<gallery_dl_rs::booru::Booru>>; 4],
    mastodon: OnceCell<Result<gallery_dl_rs::mastodon::Mastodon>>,
    kemono: OnceCell<Result<gallery_dl_rs::kemono::Kemono>>,
    imagehosts: [OnceCell<Result<gallery_dl_rs::imagehosts::ImageHosts>>; 4],
    tumblr: OnceCell<Result<gallery_dl_rs::tumblr::Tumblr>>,
    pixiv: OnceCell<Result<gallery_dl_rs::pixiv::Pixiv>>,
    deviantart: OnceCell<Result<gallery_dl_rs::deviantart::DeviantArt>>,
    flickr: OnceCell<Result<gallery_dl_rs::flickr::Flickr>>,
    wallhaven: OnceCell<Result<gallery_dl_rs::wallhaven::Wallhaven>>,
    artstation: OnceCell<Result<gallery_dl_rs::artstation::ArtStation>>,
    pinterest: OnceCell<Result<gallery_dl_rs::pinterest::Pinterest>>,
    newgrounds: OnceCell<Result<gallery_dl_rs::newgrounds::Newgrounds>>,
    furaffinity: OnceCell<Result<gallery_dl_rs::furaffinity::FurAffinity>>,
    weasyl: OnceCell<Result<gallery_dl_rs::weasyl::Weasyl>>,
    vsco: OnceCell<Result<gallery_dl_rs::vsco::Vsco>>,
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

fn extract_input<'a>(
    source: String,
    extractors: &'a Extractors,
    backend: &'a Backend,
    extractor: Extractor,
    settings: &'a BTreeMap<&str, Settings>,
) -> BoxStream<'a, Result<ExtractionEvent>> {
    stream::once(async move {
        let url = extract::http_url(&source)?;
        let input: BoxStream<'a, Result<ExtractionEvent>> = if let Some(category) =
            routed_site(&url, extractor)
        {
            let configured = &settings[category];
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
                                    native
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
                            gallery_dl_rs::reddit::Reddit::new(backend.timeout).and_then(|native| {
                                native
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Reddit initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "mastodon" => {
                    let native = extractors
                        .mastodon
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
                                    native
                                        .with_selection(gallery_dl_rs::mastodon::SelectionOptions {
                                            reblogs: options.reblogs.unwrap_or(false),
                                            replies: options.replies
                                                != Some(gallery_dl_rs::twitter::ReplyMode::Skip),
                                            text_posts: options.text_posts.unwrap_or(false),
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
                    let native = extractors
                        .kemono
                        .get_or_init(|| async {
                            gallery_dl_rs::kemono::Kemono::new(backend.timeout).and_then(|native| {
                                native
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
                                    .with_api_key(options.api_key.as_deref())
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Tumblr initialization failed: {e}")))?;
                    native.events(&url)
                }
                Extractor::Native if category == "flickr" => {
                    let native = extractors
                        .flickr
                        .get_or_init(|| async {
                            gallery_dl_rs::flickr::Flickr::new(backend.timeout).and_then(|native| {
                                native
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
                Extractor::Native if category == "pixiv" => {
                    let native = extractors
                        .pixiv
                        .get_or_init(|| async {
                            gallery_dl_rs::pixiv::Pixiv::new(backend.timeout).and_then(|native| {
                                native
                                    .with_refresh_token(options.refresh_token.as_deref())
                                    .with_retries(options.retries.unwrap())
                                    .with_request_timeout(options.timeout.unwrap())
                                    .with_network(&configured.network)
                            })
                        })
                        .await
                        .as_ref()
                        .map_err(|e| error(format!("native Pixiv initialization failed: {e}")))?;
                    native.events(&url)
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
                    let native = extractors.booru[site as usize]
                        .get_or_init(|| async {
                            gallery_dl_rs::booru::Booru::new(site, backend.timeout).and_then(
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
                            browser: configured.browser_spec.clone(),
                            proxy: configured.network.proxy().map(str::to_owned),
                            user_agent: configured.network.user_agent().map(str::to_owned),
                            retweets: (category == "twitter")
                                .then_some(options.retweets)
                                .flatten(),
                            quoted: (category == "twitter").then_some(options.quoted).flatten(),
                            expand: (category == "twitter").then_some(options.expand).flatten(),
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
                            cards: (category == "twitter").then_some(options.cards).flatten(),
                            cards_blacklist: (category == "twitter")
                                .then(|| options.cards_blacklist.clone())
                                .flatten(),
                            videos: (category == "twitter").then_some(options.videos).flatten(),
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
                        },
                    )
                }
            }
        } else if let Some(item) = extract::direct(&url) {
            stream::iter(event::group(vec![item], "direct").into_iter().map(Ok)).boxed()
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
    browser: Option<FirefoxProfile>,
    browser_spec: Option<String>,
    filename: Option<FilenameTemplate>,
    directory: Option<DirectoryTemplate>,
    selection: Option<RangeSelection>,
    filter: Option<FilterExpression>,
    post_selection: Option<PostSelection>,
    dates: DateFilter,
    network: NetworkOptions,
    download_client: OnceCell<Result<download::DownloadClient>>,
    limits: SizeLimits,
}

async fn resolved_settings(args: &Args) -> Result<BTreeMap<&'static str, Settings>> {
    let discovered = if args.native_config.is_none() && !args.native_config_ignore {
        default_native_config()
    } else {
        None
    };
    let config = match args.native_config.as_ref().or(discovered.as_ref()) {
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
        filename_conditions: None,
        file_range: args.file_range.clone(),
        file_filter: (!args.file_filter.is_empty()).then(|| args.file_filter.clone()),
        post_range: args.post_range.clone(),
        post_filter: (!args.post_filter.is_empty()).then(|| args.post_filter.clone()),
        date_before: args.date_before,
        date_after: args.date_after,
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
        api_key: None,
        refresh_token: None,
        client_id: None,
        client_secret: None,
        proxy: args.proxy.clone(),
        user_agent: args.user_agent.clone(),
        archive: args.download_archive.clone(),
        filesize_min: args.filesize_min,
        filesize_max: args.filesize_max,
        retries: args.retries,
        timeout: args.timeout,
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
        "kemono",
        "coomer",
        "pixeldrain",
        "catbox",
        "imgbox",
        "postimg",
        "tumblr",
        "pixiv",
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
    ] {
        let options = defaults
            .overlay(&config.resolve(category)?)
            .overlay(&overrides);
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
            Some(conditions) => Some(FilenameTemplate::parse_conditional(
                conditions,
                options.filename.as_deref(),
            )?),
            None => options
                .filename
                .as_deref()
                .map(FilenameTemplate::parse)
                .transpose()?,
        };
        let directory = match &options.directory_conditions {
            Some(conditions) => Some(DirectoryTemplate::parse_conditional(
                conditions,
                options.directory.as_deref(),
            )?),
            None => options
                .directory
                .as_deref()
                .map(DirectoryTemplate::parse)
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
        let dates = DateFilter::new(options.date_before, options.date_after)?;
        settings.insert(
            category,
            Settings {
                options,
                browser,
                browser_spec,
                filename,
                directory,
                selection,
                filter,
                post_selection,
                dates,
                network,
                download_client: OnceCell::new(),
                limits,
            },
        );
    }
    Ok(settings)
}
