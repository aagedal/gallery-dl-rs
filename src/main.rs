use clap::{Parser, ValueEnum};
use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    Result,
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions, Outcome},
    error,
    extract::{self, Backend},
    instagram::Instagram,
    naming::FilenameTemplate,
};
use sha2::{Digest, Sha256};
use std::{cell::Cell, collections::HashSet, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Rust gallery downloader with an optional native Instagram extractor"
)]
struct Args {
    /// Instagram, Facebook, Twitter/X, or direct media URLs
    #[arg(required = true)]
    urls: Vec<String>,
    #[arg(short, long, default_value = "downloads")]
    destination: PathBuf,
    /// Maximum simultaneous downloads
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=32))]
    jobs: u8,
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
    /// Retain validated partial HTTP media for retries and later invocations
    #[arg(long, conflicts_with_all = ["get_urls", "dump_json"])]
    resume: bool,
    /// Netscape session cookies for social extraction
    #[arg(short = 'C', long)]
    cookies: Option<PathBuf>,
    /// Social extractor; native currently supports Instagram single posts only
    #[arg(long, value_enum, default_value_t = Extractor::Compatibility)]
    extractor: Extractor,
    /// Explicit gallery-dl configuration file (default config is ignored)
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[arg(long, default_value = "gallery-dl")]
    extractor_command: PathBuf,
    /// Total seconds allowed for each social URL's extraction
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..))]
    extraction_timeout: u64,
    /// Seconds allowed per media attempt and per rate-limit wait
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
    /// Additional download/native extraction attempts on rate limits, server or transport errors
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=10))]
    retries: u8,
}

#[derive(Clone, Copy, ValueEnum)]
enum Extractor {
    Compatibility,
    Native,
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
    let filename = args
        .filename
        .as_deref()
        .map(FilenameTemplate::parse)
        .transpose()?;
    let archive = match &args.download_archive {
        Some(path) => Some(Archive::open(path).await?),
        None => None,
    };
    let native = if matches!(args.extractor, Extractor::Native) {
        Some(
            Instagram::new(
                args.cookies.as_deref(),
                Duration::from_secs(args.extraction_timeout),
            )
            .await?
            .with_retries(args.retries),
        )
    } else {
        None
    };
    let backend = Backend {
        executable: args.extractor_command,
        cookies: args.cookies,
        config: args.config,
        timeout: Duration::from_secs(args.extraction_timeout),
    };
    let failed = Cell::new(0);
    let mut seen = HashSet::new();
    // Pull one input at a time, retaining only its results and active downloads.
    // buffer_unordered below supplies backpressure when all download slots are full.
    let media = stream::iter(args.urls.iter().enumerate())
        .then(|(index, source)| {
            let native = &native;
            let backend = &backend;
            let failed = &failed;
            async move {
                let result = extract_input(source, native.as_ref(), backend).await;
                match result {
                    Ok(items) => items,
                    Err(e) => {
                        eprintln!("extraction failed for input {}: {e}", index + 1);
                        failed.set(failed.get() + 1);
                        Vec::new()
                    }
                }
            }
        })
        .flat_map(stream::iter)
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
        let client = download::client(Duration::from_secs(args.timeout))?.with_resume(args.resume);
        let results = media
            .map(|item| {
                let client = &client;
                let destination = &args.destination;
                let archive = &archive;
                let filename = filename.as_ref();
                let options = DownloadOptions {
                    retries: args.retries,
                    write_metadata: args.write_metadata,
                };
                async move {
                    match archive {
                        Some(archive) => {
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

async fn extract_input(
    source: &str,
    native: Option<&Instagram>,
    backend: &Backend,
) -> Result<Vec<extract::Media>> {
    let url = extract::http_url(source)?;
    // Social pages always go through their extractor, even if their path
    // happens to end with a media extension.
    if extract::social_site(&url).is_some() {
        match native {
            Some(native) if extract::social_site(&url) == Some("instagram") => {
                native.extract(&url).await
            }
            Some(_) => Err(error(
                "native extraction currently supports Instagram single posts only; use --extractor compatibility",
            )),
            None => backend.extract(&url).await,
        }
    } else if let Some(item) = extract::direct(&url) {
        Ok(vec![item])
    } else {
        Err(error(
            "unsupported URL; expected a social URL or direct media file",
        ))
    }
}
