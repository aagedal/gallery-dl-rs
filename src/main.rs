use clap::Parser;
use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    Result,
    download::{self, Outcome},
    error,
    extract::{self, Backend},
};
use std::{collections::HashSet, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Rust gallery downloader (social extraction currently requires gallery-dl)"
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
    /// Netscape cookie file passed to the social extraction backend
    #[arg(short = 'C', long)]
    cookies: Option<PathBuf>,
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

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
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
                backend.extract(&url).await
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
    media.retain(|item| seen.insert(item.url.to_string()));
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
