//! Video-page and HLS/DASH downloads through an external `yt-dlp`, like
//! gallery-dl's `ytdl` downloader. The native port
//! [yt-dlp-rs](https://github.com/aagedal/yt-dlp-rs) accepts the same
//! arguments and is used when the default `yt-dlp` is not on `PATH`.
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
    pub cookies: Option<&'a crate::cookies::YtdlCookies>,
}

/// Upstream `downloader.ytdl` options: `format`, `cmdline-args` and
/// `config-file`. `raw-options` (Python keyword arguments) has no
/// command-line equivalent.
#[derive(Debug, Default)]
pub struct YtdlOptions {
    pub format: Option<String>,
    pub cmdline_args: Vec<String>,
    pub config_file: Option<std::path::PathBuf>,
    /// `forward-cookies` (default true).
    pub forward_cookies: bool,
    /// `raw-options` translated to command-line flags.
    pub raw_args: Vec<String>,
    /// `logging` (default true) and `deprecations` (default false).
    pub logging: bool,
    pub deprecations: bool,
}

/// Re-log yt-dlp's `WARNING:`/`ERROR:` lines as `[ytdl][LEVEL]` messages,
/// like upstream's logging bridge; other output is debug-level and hidden.
async fn relay_messages(stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let (level, message) = if let Some(message) = line.strip_prefix("ERROR: ") {
            (crate::actions::ERROR, message)
        } else if let Some(message) = line.strip_prefix("WARNING: ") {
            if !YtdlOptions::deprecations() && message.to_ascii_lowercase().contains("deprecat") {
                continue;
            }
            (crate::actions::WARNING, message)
        } else {
            continue;
        };
        let _ = crate::actions::log_as("ytdl", level, message);
    }
}

/// Translate `raw-options` (YoutubeDL constructor options) into yt-dlp
/// command-line flags. Unknown options are reported and ignored.
fn raw_options(object: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    use serde_json::Value;
    let mut args = Vec::new();
    for (key, value) in object {
        let text = match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Array(items) => Some(
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            _ => None,
        };
        let enabled = !matches!(value, Value::Null | Value::Bool(false));
        let flag = |name: &str| enabled.then(|| vec![name.to_owned()]);
        let with = |name: &str| text.clone().map(|t| vec![name.to_owned(), t]);
        let mapped = match key.as_str() {
            "quiet" => flag("--quiet"),
            "no_warnings" => flag("--no-warnings"),
            "verbose" => flag("--verbose"),
            "writesubtitles" => flag("--write-subs"),
            "writeautomaticsub" => flag("--write-auto-subs"),
            "allsubtitles" => Some(vec!["--sub-langs".to_owned(), "all".to_owned()]),
            "subtitleslangs" => with("--sub-langs"),
            "subtitlesformat" => with("--sub-format"),
            "writethumbnail" => flag("--write-thumbnail"),
            "writedescription" => flag("--write-description"),
            "writeinfojson" => flag("--write-info-json"),
            "merge_output_format" => with("--merge-output-format"),
            "ratelimit" => with("--limit-rate"),
            "retries" => with("--retries"),
            "fragment_retries" => with("--fragment-retries"),
            "concurrent_fragment_downloads" => with("--concurrent-fragments"),
            "socket_timeout" => with("--socket-timeout"),
            "nocheckcertificate" => flag("--no-check-certificates"),
            "geo_bypass" => flag("--geo-bypass"),
            "geo_bypass_country" => with("--geo-bypass-country"),
            "noplaylist" => flag("--no-playlist"),
            "playlistend" => with("--playlist-end"),
            "max_filesize" => with("--max-filesize"),
            "min_filesize" => with("--min-filesize"),
            "username" => with("--username"),
            "password" => with("--password"),
            "videopassword" => with("--video-password"),
            "cookiefile" => with("--cookies"),
            "proxy" => with("--proxy"),
            "source_address" => with("--source-address"),
            "http_chunk_size" => with("--http-chunk-size"),
            "external_downloader" => with("--downloader"),
            "keepvideo" => flag("--keep-video"),
            "prefer_free_formats" => flag("--prefer-free-formats"),
            "embedsubtitles" => flag("--embed-subs"),
            "embedthumbnail" => flag("--embed-thumbnail"),
            "addmetadata" => flag("--embed-metadata"),
            _ => {
                eprintln!("warning: unsupported ytdl raw-option '{key}'");
                None
            }
        };
        args.extend(mapped.unwrap_or_default());
    }
    args
}

static OPTIONS: std::sync::OnceLock<YtdlOptions> = std::sync::OnceLock::new();

impl YtdlOptions {
    /// Parse a `downloader.ytdl` object.
    pub fn parse(value: &serde_json::Value) -> crate::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| crate::error("downloader.ytdl must be an object"))?;
        let cmdline_args = match object.get("cmdline-args") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::String(text)) => {
                text.split_whitespace().map(str::to_owned).collect()
            }
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| crate::error("cmdline-args must be strings"))
                })
                .collect::<crate::Result<_>>()?,
            Some(_) => return Err(crate::error("cmdline-args must be a string or list")),
        };
        Ok(Self {
            format: object
                .get("format")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            cmdline_args,
            config_file: object
                .get("config-file")
                .and_then(serde_json::Value::as_str)
                .map(crate::postprocess::expand_path),
            forward_cookies: object.get("forward-cookies") != Some(&serde_json::Value::Bool(false)),
            logging: object.get("logging") != Some(&serde_json::Value::Bool(false)),
            deprecations: object
                .get("deprecations")
                .is_some_and(|v| v.as_bool() == Some(true)),
            raw_args: object
                .get("raw-options")
                .and_then(serde_json::Value::as_object)
                .map(raw_options)
                .unwrap_or_default(),
        })
    }

    /// Install process-wide options; later calls are ignored.
    pub fn install(self) {
        let _ = OPTIONS.set(self);
    }

    /// Arguments added to every yt-dlp invocation.
    pub fn arguments() -> Vec<String> {
        let Some(options) = OPTIONS.get() else {
            return Vec::new();
        };
        let mut args = Vec::new();
        if let Some(file) = &options.config_file {
            args.push("--config-locations".into());
            args.push(file.display().to_string());
        }
        args.extend(options.raw_args.iter().cloned());
        args.extend(options.cmdline_args.iter().cloned());
        args
    }

    /// Upstream `logging` (default true): relay yt-dlp's messages.
    pub fn logging() -> bool {
        OPTIONS.get().is_none_or(|options| options.logging)
    }

    /// Upstream `deprecations` (default false): show deprecation warnings.
    pub fn deprecations() -> bool {
        OPTIONS.get().is_some_and(|options| options.deprecations)
    }

    /// Whether session cookies are passed to yt-dlp.
    pub fn forward_cookies() -> bool {
        OPTIONS.get().is_none_or(|options| options.forward_cookies)
    }

    fn format() -> Option<&'static str> {
        OPTIONS.get()?.format.as_deref()
    }
}

fn extension_name(request: &Request<'_>) -> String {
    request
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// Command-line arguments, excluding the executable.
pub(super) fn arguments(
    request: &Request<'_>,
    cookies: &[String],
    output: &Path,
    url: &url::Url,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "--ignore-config",
        "--no-playlist",
        "--no-progress",
        // Warnings stay on: upstream shows them (or relays them, `logging`).
        "--quiet",
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
        args.push(YtdlOptions::format().unwrap_or("bestaudio/best").into());
        args.push("--extract-audio".into());
        args.push("--audio-format".into());
        args.push(extension);
    }
    if !AUDIO_CONTAINERS.contains(&extension_name(request).as_str())
        && let Some(format) = YtdlOptions::format()
    {
        args.push("--format".into());
        args.push(format.into());
    }
    for (name, value) in &request.media.headers {
        if SKIPPED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        args.push("--add-header".into());
        args.push(format!("{name}:{value}"));
    }
    args.extend_from_slice(cookies);
    args.extend(YtdlOptions::arguments());
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
            "video downloads need yt-dlp or yt-dlp-rs on PATH, or --ytdl-command",
            false,
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let work = tempfile::Builder::new()
        .prefix(".ytdl-")
        .tempdir_in(parent)?;
    // Cookie copies live outside the destination directory.
    let scratch = tempfile::tempdir()?;
    let cookies = match request.cookies.filter(|_| YtdlOptions::forward_cookies()) {
        Some(cookies) => cookies.arguments(scratch.path()).map_err(|error| Failure {
            error,
            retry: false,
            fallback: true,
        })?,
        None => Vec::new(),
    };
    let mut command = tokio::process::Command::new(command);
    let logging = YtdlOptions::logging();
    command
        .args(arguments(&request, &cookies, work.path(), url))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    // Upstream `logging: false` writes yt-dlp's output directly.
    if logging {
        command.stdout(Stdio::null()).stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }
    let status = match async {
        let mut child = command.spawn()?;
        if let Some(stderr) = child.stderr.take() {
            relay_messages(stderr).await;
        }
        child.wait().await
    }
    .await
    {
        Ok(status) => status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(failure(
                "video downloads need yt-dlp or yt-dlp-rs on PATH, or --ytdl-command",
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

/// Default yt-dlp command, and the native port tried in its place.
const DEFAULT_COMMAND: &str = "yt-dlp";
const NATIVE_COMMAND: &str = "yt-dlp-rs";

/// The executable to run for a yt-dlp command: the given path when it names
/// an existing file, or a bare name resolved on `PATH`. The default `yt-dlp`
/// falls back to `yt-dlp-rs`, which takes the same arguments.
pub fn executable(command: &Path) -> Option<PathBuf> {
    executable_in(command, std::env::var_os("PATH").as_deref())
}

fn executable_in(command: &Path, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    find(command, path).or_else(|| {
        (command == Path::new(DEFAULT_COMMAND))
            .then(|| find(Path::new(NATIVE_COMMAND), path))
            .flatten()
    })
}

/// The command a `ytdl.module` setting names, if it is one this port runs.
/// Upstream names Python modules (`yt_dlp`); `yt_dlp_rs` selects the port.
pub fn module_command(module: &str) -> Option<&'static str> {
    match module.to_ascii_lowercase().replace('-', "_").as_str() {
        "yt_dlp" => Some(DEFAULT_COMMAND),
        "yt_dlp_rs" => Some(NATIVE_COMMAND),
        _ => None,
    }
}

fn find(command: &Path, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
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
    std::env::split_paths(path?)
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
    #[test]
    fn upstream_ytdl_options_parse() {
        let options = YtdlOptions::parse(&serde_json::json!({
            "format": "bv*+ba",
            "cmdline-args": "--embed-subs --sub-langs en",
            "config-file": "/tmp/yt.conf",
            "raw-options": {"ignored": true}
        }))
        .unwrap();
        assert_eq!(options.format.as_deref(), Some("bv*+ba"));
        assert_eq!(options.cmdline_args, ["--embed-subs", "--sub-langs", "en"]);
        assert_eq!(
            options.config_file.unwrap(),
            std::path::Path::new("/tmp/yt.conf")
        );
        assert!(YtdlOptions::parse(&serde_json::json!({"cmdline-args": 1})).is_err());
    }

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
            cookies: None,
        };
        let cookies = ["--cookies".to_owned(), "/tmp/c.txt".to_owned()];
        let args = arguments(&request, &cookies, Path::new("/out/a/.ytdl-x"), &page);
        let joined = args.join(" ");
        assert!(joined.starts_with("--ignore-config --no-playlist"));
        assert!(joined.contains("--retries 3 --limit-rate 1000 --proxy http://proxy:8080"));
        assert!(joined.contains("--merge-output-format mkv --remux-video mkv"));
        assert!(joined.contains("--add-header Referer:https://site.example/"));
        assert!(!joined.contains("Range"));
        assert!(joined.contains("--cookies /tmp/c.txt --output "));
        assert_eq!(&args[args.len() - 2..], ["--", page.as_str()]);
        let template = Path::new("/out/a/.ytdl-x").join("media.%(ext)s");
        assert!(args.contains(&template.display().to_string()));
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
    fn default_command_falls_back_to_yt_dlp_rs() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join(if cfg!(windows) {
            "yt-dlp-rs.exe"
        } else {
            "yt-dlp-rs"
        });
        std::fs::write(&native, b"").unwrap();
        let path = Some(dir.path().as_os_str());
        assert_eq!(
            executable_in(Path::new("yt-dlp"), path),
            Some(native.clone())
        );
        // Explicit other commands never fall back.
        assert_eq!(executable_in(Path::new("youtube-dl"), path), None);
        assert_eq!(executable_in(&dir.path().join("yt-dlp"), path), None);
        // An installed yt-dlp is preferred.
        let python = dir.path().join(if cfg!(windows) {
            "yt-dlp.exe"
        } else {
            "yt-dlp"
        });
        std::fs::write(&python, b"").unwrap();
        assert_eq!(executable_in(Path::new("yt-dlp"), path), Some(python));
    }

    #[test]
    fn modules_name_commands() {
        assert_eq!(module_command("yt_dlp"), Some("yt-dlp"));
        assert_eq!(module_command("yt-dlp-rs"), Some("yt-dlp-rs"));
        assert_eq!(module_command("YT_DLP_RS"), Some("yt-dlp-rs"));
        assert_eq!(module_command("youtube_dl"), None);
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
            "#!/bin/sh\nfor a; do case \"$prev\" in --output) out=\"$a\";; \
             --cookies) jar=\"$a\";; esac; prev=\"$a\"; done\n\
             dest=\"$(echo \"$out\" | sed 's/%(ext)s/mp4/')\"\n\
             if [ -n \"$jar\" ]; then cat \"$jar\" > \"$dest\"; else printf video > \"$dest\"; fi\n",
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
            cookies: None,
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
        // Cookie files reach yt-dlp as a copy outside the destination.
        let jar = scripts.path().join("jar.txt");
        std::fs::write(&jar, "jar").unwrap();
        let cookies = crate::cookies::YtdlCookies::File(jar);
        let with_cookies = Request {
            path: &out.path().join("c.mp4"),
            cookies: Some(&cookies),
            ..request()
        };
        assert!(matches!(
            download(with_cookies).await.unwrap(),
            Outcome::Downloaded(_)
        ));
        assert_eq!(std::fs::read(out.path().join("c.mp4")).unwrap(), b"jar");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 2);
    }
}

#[cfg(test)]
mod raw_option_tests {
    #[test]
    fn translates_raw_options_and_cookie_forwarding() {
        let options = super::YtdlOptions::parse(&serde_json::json!({
            "forward-cookies": false,
            "raw-options": {"quiet": true, "writesubtitles": false,
                            "merge_output_format": "mkv", "subtitleslangs": ["en", "de"]},
        }))
        .unwrap();
        assert!(!options.forward_cookies);
        assert_eq!(
            options.raw_args,
            [
                "--quiet",
                "--merge-output-format",
                "mkv",
                "--sub-langs",
                "en,de"
            ]
        );
    }
}
