use clap::{Parser, ValueEnum};
use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    Result,
    download::{self, Outcome},
    error,
    extract::{self, Backend},
    instagram::Instagram,
};
use std::{collections::HashSet, path::PathBuf, time::Duration};

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
    /// Seconds allowed for each social URL's extraction
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..))]
    extraction_timeout: u64,
    /// Seconds allowed for each media download attempt
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
    /// Additional attempts on server or transport errors
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
    if let Err(e) = run(Args::parse()).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
    if matches!(args.extractor, Extractor::Native) && args.config.is_some() {
        return Err(error(
            "--config is only supported with --extractor compatibility",
        ));
    }
    let native = if matches!(args.extractor, Extractor::Native) {
        Some(
            Instagram::new(
                args.cookies.as_deref(),
                Duration::from_secs(args.extraction_timeout),
            )
            .await?,
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
    let mut media = Vec::new();
    let mut failed = 0;
    for (index, source) in args.urls.iter().enumerate() {
        let result = async {
            let url = extract::http_url(source)?;
            // Social pages always go through their extractor, even if their path
            // happens to end with a media extension.
            if extract::social_site(&url).is_some() {
                match &native {
                    Some(native) if extract::social_site(&url) == Some("instagram") => native.extract(&url).await,
                    Some(_) => Err(error("native extraction currently supports Instagram single posts only; use --extractor compatibility")),
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
        .await;
        match result {
            Ok(items) => media.extend(items),
            Err(e) => {
                eprintln!("extraction failed for input {}: {e}", index + 1);
                failed += 1;
            }
        }
    }
    let mut seen = HashSet::new();
    media.retain(|item| seen.insert(item.identity()));
    if args.dump_json {
        println!("{}", serde_json::to_string_pretty(&media)?);
    } else if args.get_urls {
        for item in &media {
            println!("{}", item.url);
        }
    } else {
        let client = download::client(Duration::from_secs(args.timeout))?;
        let results = stream::iter(
            media
                .iter()
                .map(|item| download::download(&client, &args.destination, item, args.retries)),
        )
        .buffer_unordered(args.jobs as usize);
        tokio::pin!(results);
        while let Some(result) = results.next().await {
            match result {
                Ok(Outcome::Downloaded(path)) => eprintln!("downloaded {}", path.display()),
                Ok(Outcome::Skipped(path)) => eprintln!("skipped {}", path.display()),
                Err(e) => {
                    eprintln!("download failed: {e}");
                    failed += 1;
                }
            }
        }
    }
    if failed > 0 {
        Err(error(format!("{failed} operation(s) failed")))
    } else {
        Ok(())
    }
}
