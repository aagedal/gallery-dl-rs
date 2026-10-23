//! Video-page and HLS/DASH downloads through an external `yt-dlp`, like
//! gallery-dl's `ytdl` downloader.
//!
//! The executable is invoked directly, without a shell, with user yt-dlp
//! configuration files ignored as upstream's module use does. Output goes to
//! a private temporary directory beside the destination and is published
//! without replacing existing files. Diagnostics never include URLs,
//! headers or yt-dlp's own output, which can echo signed URLs.
use super::{Failure, Outcome};
use crate::{error, extract::Media, extract::MediaJob, network::NetworkOptions, size::SizeLimits};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::SystemTime,
};

/// Containers yt-dlp can merge or remux into, chosen by the destination's
/// extension so the published file matches its name.
const VIDEO_CONTAINERS: &[&str] = &["mp4", "mkv", "webm", "mov", "flv", "avi"];
const AUDIO_CONTAINERS: &[&str] = &["m4a", "mp3", "ogg", "opus", "flac", "wav", "aac"];
const STEM: &str = "media";

/// Headers that transport code owns; never forwarded to yt-dlp.
const SKIPPED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "proxy-authorization",
    "range",
    "if-range",
];

/// The URL yt-dlp should read, or `None` for jobs it cannot take.
/// Inline manifests have no fetchable location for relative segment URLs.
pub(super) fn source(media: &Media) -> Option<&url::Url> {
    match &media.job {
        MediaJob::VideoPage { page_url } => Some(page_url),
        MediaJob::Manifest {
            base_url,
            inline_data: None,
            ..
        } => Some(base_url),
        _ => None,
    }
}

pub(super) struct Request<'a> {
    pub command: &'a Path,
    pub media: &'a Media,
    pub path: &'a Path,
    pub network: &'a NetworkOptions,
    pub retries: u8,
    pub rate: Option<u64>,
    pub limits: SizeLimits,
    pub mtime: Option<SystemTime>,
}

/// Command-line arguments, excluding the executable.
pub(super) fn arguments(request: &Request<'_>, output: &Path, url: &url::Url) -> Vec<String> {
    let mut args: Vec<String> = [
        "--ignore-config",
        "--no-playlist",
        "--no-progress",
        "--quiet",
        "--no-warnings",
        "--no-mtime",
        "--no-part",
        "--no-cache-dir",
        "--no-write-comments",
        "--no-exec",
    ]
    .map(str::to_owned)
    .into();
    args.push("--retries".into());
    args.push(request.retries.to_string());
    if let Some(rate) = request.rate {
        args.push("--limit-rate".into());
        args.push(rate.to_string());
    }
    if let Some(proxy) = request.network.proxy() {
        args.push("--proxy".into());
        args.push(proxy.to_owned());
    }
    let extension = request
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if VIDEO_CONTAINERS.contains(&extension.as_str()) {
        args.push("--merge-output-format".into());
        args.push(extension.clone());
        args.push("--remux-video".into());
        args.push(extension);
    } else if AUDIO_CONTAINERS.contains(&extension.as_str()) {
        args.push("--format".into());
        args.push("bestaudio/best".into());
        args.push("--extract-audio".into());
        args.push("--audio-format".into());
        args.push(extension);
    }
    for (name, value) in &request.media.headers {
        if SKIPPED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        args.push("--add-header".into());
        args.push(format!("{name}:{value}"));
    }
    args.push("--output".into());
    args.push(output.join(format!("{STEM}.%(ext)s")).display().to_string());
    // URLs never start with '-', but keep them out of option parsing anyway.
    args.push("--".into());
    args.push(url.as_str().to_owned());
    args
}

pub(super) async fn download(request: Request<'_>) -> std::result::Result<Outcome, Failure> {
    let failure = |message: &str, retry: bool| Failure {
        error: error(message),
        retry,
        fallback: true,
    };
    let url = source(request.media)
        .ok_or_else(|| failure("inline manifests cannot be downloaded with yt-dlp", false))?;
    let parent = request
        .path
        .parent()
        .ok_or_else(|| failure("invalid destination", false))?;
    // Fail before touching the filesystem when yt-dlp is unavailable.
    let command = executable(request.command).ok_or_else(|| {
        failure(
            "video downloads need yt-dlp on PATH or --ytdl-command",
            false,
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let work = tempfile::Builder::new()
        .prefix(".ytdl-")
        .tempdir_in(parent)?;
    let mut command = tokio::process::Command::new(command);
    command
        .args(arguments(&request, work.path(), url))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = match command.status().await {
        Ok(status) => status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(failure(
                "video downloads need yt-dlp on PATH or --ytdl-command",
                false,
            ));
        }
        Err(_) => return Err(failure("could not start yt-dlp", false)),
    };
    if !status.success() {
        return Err(failure("yt-dlp could not download the video", true));
    }
    let output = produced_file(work.path())?
        .ok_or_else(|| failure("yt-dlp produced no media file", false))?;
    let size = std::fs::metadata(&output)?.len();
    if let Some(rejection) = request.limits.check(size) {
        return Ok(Outcome::Filtered(rejection));
    }
    if let Some(mtime) = request.mtime {
        std::fs::File::options()
            .write(true)
            .open(&output)?
            .set_modified(mtime)?;
    }
    // A hard link publishes atomically and never replaces an existing file;
    // the temporary directory, including the source name, is removed on drop.
    match std::fs::hard_link(&output, request.path) {
        Ok(()) => Ok(Outcome::Downloaded(request.path.into())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(request.path)?.is_file() {
                Ok(Outcome::Skipped(request.path.into()))
            } else {
                Err(failure("media destination is not a regular file", false))
            }
        }
        Err(_) => {
            // Filesystems without hard links: copy, then refuse to clobber.
            let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
            std::fs::copy(&output, &temporary)?;
            match temporary.persist_noclobber(request.path) {
                Ok(_) => Ok(Outcome::Downloaded(request.path.into())),
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(Outcome::Skipped(request.path.into()))
                }
                Err(e) => Err(e.error.into()),
            }
        }
    }
}

/// The executable a bare command name resolves to on `PATH`, or the given
/// path when it names an existing file.
fn executable(command: &Path) -> Option<PathBuf> {
    if command.components().count() > 1 {
        return command.is_file().then(|| command.to_owned());
    }
    let names: Vec<std::ffi::OsString> = if cfg!(windows) && command.extension().is_none() {
        [".exe", ".cmd", ".bat", ""]
            .iter()
            .map(|ext| {
                let mut name = command.as_os_str().to_owned();
                name.push(ext);
                name
            })
            .collect()
    } else {
        vec![command.as_os_str().to_owned()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// The single completed `media.EXT` file yt-dlp wrote.
fn produced_file(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut found = None;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(ext) = name
            .strip_prefix(STEM)
            .and_then(|rest| rest.strip_prefix('.'))
        else {
            continue;
        };
        if ext.is_empty() || ext.contains('.') || matches!(ext, "part" | "ytdl" | "temp") {
            continue;
        }
        if entry.file_type()?.is_file() {
            found = Some(entry.path());
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn media(job: MediaJob) -> Media {
        Media {
            url: url::Url::parse("https://cdn.example/v.mp4").unwrap(),
            job,
            category: "test".into(),
            filename: "v.mp4".into(),
            metadata: None,
            headers: BTreeMap::from([
                ("Referer".into(), "https://site.example/".into()),
                ("Range".into(), "bytes=0-".into()),
            ]),
            handoff: BTreeMap::new(),
        }
    }

    #[test]
    fn builds_arguments_for_the_destination_container() {
        let page = url::Url::parse("https://site.example/watch?v=1").unwrap();
        let media = media(MediaJob::VideoPage {
            page_url: page.clone(),
        });
        let network = NetworkOptions::new(Some("http://proxy:8080".into()), None).unwrap();
        let request = Request {
            command: Path::new("yt-dlp"),
            media: &media,
            path: Path::new("/out/a/v.MKV"),
            network: &network,
            retries: 3,
            rate: Some(1000),
            limits: SizeLimits::default(),
            mtime: None,
        };
        let args = arguments(&request, Path::new("/out/a/.ytdl-x"), &page);
        let joined = args.join(" ");
        assert!(joined.starts_with("--ignore-config --no-playlist"));
        assert!(joined.contains("--retries 3 --limit-rate 1000 --proxy http://proxy:8080"));
        assert!(joined.contains("--merge-output-format mkv --remux-video mkv"));
        assert!(joined.contains("--add-header Referer:https://site.example/"));
        assert!(!joined.contains("Range"));
        assert_eq!(&args[args.len() - 2..], ["--", page.as_str()]);
        assert!(args.contains(&"/out/a/.ytdl-x/media.%(ext)s".to_owned()));
    }

    #[test]
    fn inline_manifests_and_direct_jobs_have_no_source() {
        let base = url::Url::parse("https://cdn.example/m.mpd").unwrap();
        let inline = media(MediaJob::Manifest {
            protocol: crate::extract::ManifestProtocol::Dash,
            base_url: base.clone(),
            inline_data: Some("<MPD/>".into()),
        });
        assert!(source(&inline).is_none());
        assert!(source(&media(MediaJob::DirectHttp)).is_none());
        let remote = media(MediaJob::Manifest {
            protocol: crate::extract::ManifestProtocol::Hls,
            base_url: base.clone(),
            inline_data: None,
        });
        assert_eq!(source(&remote), Some(&base));
    }

    #[test]
    fn picks_the_completed_output_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("media.mp4.part"), b"x").unwrap();
        std::fs::write(dir.path().join("media.f137.mp4"), b"x").unwrap();
        assert_eq!(produced_file(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join("media.mp4"), b"x").unwrap();
        assert_eq!(
            produced_file(dir.path()).unwrap(),
            Some(dir.path().join("media.mp4"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_the_command_and_publishes_its_output() {
        use std::os::unix::fs::PermissionsExt;
        let scripts = tempfile::tempdir().unwrap();
        let script = scripts.path().join("fake-ytdl");
        // Writes the output template's file, as yt-dlp would.
        std::fs::write(
            &script,
            "#!/bin/sh\nfor a; do case \"$prev\" in --output) out=\"$a\";; esac; prev=\"$a\"; done\n\
             printf video > \"$(echo \"$out\" | sed 's/%(ext)s/mp4/')\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("v.mp4");
        let media = media(MediaJob::VideoPage {
            page_url: url::Url::parse("https://site.example/watch").unwrap(),
        });
        let network = NetworkOptions::default();
        let request = || Request {
            command: &script,
            media: &media,
            path: &path,
            network: &network,
            retries: 0,
            rate: None,
            limits: SizeLimits::default(),
            mtime: None,
        };
        let outcome = download(request()).await.unwrap();
        assert!(matches!(outcome, Outcome::Downloaded(_)));
        assert_eq!(std::fs::read(&path).unwrap(), b"video");
        assert!(matches!(
            download(request()).await.unwrap(),
            Outcome::Skipped(_)
        ));
        // Only the published file remains; work directories are removed.
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1);
        let missing = Request {
            command: Path::new("/nonexistent/yt-dlp"),
            path: &out.path().join("w.mp4"),
            ..request()
        };
        let failure = download(missing).await.unwrap_err();
        assert!(failure.error.to_string().contains("--ytdl-command"));
    }
}
