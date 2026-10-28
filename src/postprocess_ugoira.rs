//! Ugoira timing strategies and cancellable external conversion processes.
use super::{FrameInput, zip};
use crate::{Result, error, extract::Media, pixiv::UgoiraFrame};
use serde_json::{Map, Value};
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, process::Command};
const MAX_FRAMES: usize = 100_000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_PROCESS_OUTPUT: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Concat,
    Image2,
    Mkvmerge,
    Archive,
}
#[derive(Clone)]
enum Rate {
    Auto,
    Uniform,
    Custom(Option<String>),
}
#[derive(Clone)]
pub struct UgoiraOptions {
    mode: Mode,
    extension: String,
    args: Vec<OsString>,
    twopass: bool,
    loglevel: Option<String>,
    capture: bool,
    keep: bool,
    skip: bool,
    repeat: bool,
    rate: Rate,
    prevent_odd: bool,
    metadata: Option<String>,
    mtime: bool,
    ffmpeg: PathBuf,
    mkvmerge: PathBuf,
    mkvm_args: Vec<OsString>,
    mkvm_mtime: bool,
    mkvm_output: bool,
    mkvm_metadata: bool,
}
impl std::fmt::Debug for UgoiraOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UgoiraOptions")
            .field("mode", &self.mode)
            .field("extension", &self.extension)
            .finish_non_exhaustive()
    }
}
fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
fn boolean(options: &Map<String, Value>, name: &str, default: bool) -> bool {
    options.get(name).map_or(default, truth)
}
fn arguments(value: Option<&Value>, default: &[&str]) -> Result<Vec<OsString>> {
    let Some(value) = value.filter(|value| truth(value)) else {
        return Ok(default.iter().map(OsString::from).collect());
    };
    let values = value
        .as_array()
        .ok_or_else(|| error("ugoira converter arguments must be a list of strings"))?;
    if values.len() > 4096 {
        return Err(error("ugoira converter arguments exceed the count limit"));
    }
    let mut bytes = 0usize;
    values
        .iter()
        .map(|value| {
            let value = value
                .as_str()
                .filter(|value| !value.contains('\0'))
                .ok_or_else(|| {
                    error("ugoira converter arguments must be strings without null bytes")
                })?;
            bytes = bytes.saturating_add(value.len());
            if bytes > 1024 * 1024 {
                return Err(error("ugoira converter arguments exceed the size limit"));
            }
            Ok(OsString::from(value))
        })
        .collect()
}
/// Python `os.path.expandvars(os.path.expanduser(raw))`.
pub(crate) fn expanded(raw: &str) -> String {
    let raw = if raw == "~" || raw.starts_with("~/") {
        std::env::var("HOME")
            .map(|home| format!("{home}{}", &raw[1..]))
            .unwrap_or_else(|_| raw.to_owned())
    } else {
        raw.to_owned()
    };
    let mut result = String::new();
    let mut rest = raw.as_str();
    while let Some((before, tail)) = rest.split_once('$') {
        result.push_str(before);
        let (name, after, token) = if let Some(tail) = tail.strip_prefix('{') {
            if let Some((name, after)) = tail.split_once('}') {
                (name, after, format!("${{{name}}}"))
            } else {
                result.push_str("${");
                rest = tail;
                continue;
            }
        } else {
            let end = tail
                .find(|value: char| !value.is_ascii_alphanumeric() && value != '_')
                .unwrap_or(tail.len());
            (&tail[..end], &tail[end..], format!("${}", &tail[..end]))
        };
        result.push_str(&std::env::var(name).unwrap_or(token));
        rest = after;
    }
    result.push_str(rest);
    result
}
fn executable(value: Option<&Value>, base: &Path, default: &str) -> Result<PathBuf> {
    let Some(value) = value.filter(|value| truth(value)) else {
        return Ok(default.into());
    };
    let raw = value
        .as_str()
        .filter(|value| value.len() <= 64 * 1024 && !value.contains('\0'))
        .ok_or_else(|| error("ugoira converter location must be a bounded path string"))?;
    let path = PathBuf::from(expanded(raw));
    Ok(if path.is_relative() && path.components().count() > 1 {
        base.join(path)
    } else {
        path
    })
}
fn detected(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| {
            let candidate = directory.join(program);
            let Ok(metadata) = candidate.metadata() else {
                return false;
            };
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
    })
}
impl UgoiraOptions {
    pub fn parse(options: &Map<String, Value>, base: &Path) -> Result<Self> {
        Self::parse_detected(options, base, detected("mkvmerge"))
    }
    fn parse_detected(
        options: &Map<String, Value>,
        base: &Path,
        mkvmerge_available: bool,
    ) -> Result<Self> {
        let explicit_extension = options.get("extension").filter(|value| !value.is_null());
        let requested = explicit_extension
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| {
                        (1..=16).contains(&value.len())
                            && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
                    })
                    .ok_or_else(|| {
                        error("ugoira extension must contain 1 to 16 ASCII letters or digits")
                    })
            })
            .transpose()?;
        let mode = options
            .get("mode")
            .filter(|value| truth(value))
            .or_else(|| options.get("ffmpeg-demuxer"));
        let auto = mode.is_none()
            || mode == Some(&Value::Null)
            || mode.and_then(Value::as_str) == Some("auto");
        let mode = match mode.and_then(Value::as_str) {
            _ if auto => {
                if matches!(requested, None | Some("webm" | "mkv"))
                    && (options.get("mkvmerge-location").is_some_and(truth) || mkvmerge_available)
                {
                    Mode::Mkvmerge
                } else {
                    Mode::Concat
                }
            }
            Some("mkvmerge") => Mode::Mkvmerge,
            Some("image2") => Mode::Image2,
            Some("archive") => Mode::Archive,
            _ => Mode::Concat,
        };
        let extension = requested
            .unwrap_or(if mode == Mode::Archive { "zip" } else { "webm" })
            .to_owned();
        let args = arguments(options.get("ffmpeg-args"), &["-pix_fmt", "yuv420p"])?;
        let mut codec = None;
        for (index, arg) in args.iter().enumerate() {
            let raw = arg.to_str().expect("parsed UTF-8 argument");
            let (arg, stream) = raw.split_once(':').unwrap_or((raw, ""));
            if arg == "-vcodec"
                || matches!(arg, "-c" | "-codec")
                    && (stream.is_empty() || matches!(stream.split(':').next(), Some("v" | "V")))
            {
                codec = Some(
                    args.get(index + 1)
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| error("ugoira video codec argument requires a value"))?,
                );
            }
        }
        let prevent_odd = boolean(options, "libx264-prevent-odd", true)
            && (matches!(codec, Some("libx264" | "libx265"))
                || codec.is_none_or(str::is_empty)
                    && matches!(extension.to_lowercase().as_str(), "mp4" | "mkv"));
        let output = options
            .get("ffmpeg-output")
            .cloned()
            .unwrap_or_else(|| Value::String("error".into()));
        let loglevel = output.as_str().map(str::to_owned);
        if loglevel
            .as_ref()
            .is_some_and(|value| value.len() > 1024 || value.contains('\0'))
        {
            return Err(error("ugoira converter output level is invalid"));
        }
        let rate = match options.get("framerate") {
            None => Rate::Auto,
            Some(Value::String(value)) if value == "auto" => Rate::Auto,
            Some(Value::String(value)) if value == "uniform" => Rate::Uniform,
            Some(value) if !truth(value) => Rate::Custom(None),
            Some(Value::String(value)) if value.len() <= 1024 && !value.contains('\0') => {
                Rate::Custom(Some(value.clone()))
            }
            Some(Value::Number(value)) => Rate::Custom(Some(value.to_string())),
            Some(Value::Bool(true)) => Rate::Custom(Some("True".into())),
            _ => {
                return Err(error(
                    "ugoira framerate must be auto, uniform, a number, or a bounded string",
                ));
            }
        };
        let metadata = match options.get("metadata") {
            Some(value) if !truth(value) => None,
            Some(Value::String(value)) => Some(value.clone()),
            _ => Some("animation.json".into()),
        };
        if metadata
            .as_ref()
            .is_some_and(|value| value.len() > 64 * 1024 || value.contains('\0'))
        {
            return Err(error("ugoira metadata filename is invalid"));
        }
        Ok(Self {
            mode,
            extension,
            args,
            twopass: boolean(options, "ffmpeg-twopass", false),
            loglevel,
            capture: truth(&output),
            keep: boolean(options, "keep-files", false),
            skip: boolean(options, "skip", true),
            repeat: boolean(options, "repeat-last-frame", true),
            rate,
            prevent_odd,
            metadata,
            mtime: boolean(options, "mtime", true),
            ffmpeg: executable(options.get("ffmpeg-location"), base, "ffmpeg")?,
            mkvmerge: executable(options.get("mkvmerge-location"), base, "mkvmerge")?,
            mkvm_args: arguments(options.get("mkvmerge-args"), &[])?,
            mkvm_mtime: boolean(options, "mkvmerge-mtime", false),
            mkvm_output: boolean(options, "mkvmerge-output", false),
            mkvm_metadata: boolean(options, "mkvmerge-metadata", true),
        })
    }
    pub fn extension(&self) -> &str {
        &self.extension
    }
    pub fn keep_files(&self) -> bool {
        self.keep
    }
    pub fn skip(&self) -> bool {
        self.skip
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn repeat_last_frame(&self) -> bool {
        self.repeat
    }
    pub fn metadata_name(&self) -> Option<&str> {
        self.metadata.as_deref()
    }
    pub fn mtime(&self) -> bool {
        self.mtime
    }
    pub fn two_pass(&self) -> bool {
        self.twopass
    }
    pub fn prevent_odd(&self) -> bool {
        self.prevent_odd
    }
    pub fn rates(&self, frames: &[UgoiraFrame]) -> Result<(Option<String>, Option<String>)> {
        if let Rate::Custom(value) = &self.rate {
            return Ok((None, value.clone()));
        }
        validate_frames(frames)?;
        Ok(match &self.rate {
            Rate::Custom(value) => (None, value.clone()),
            _ if uniform(frames) => (Some(format!("1000/{}", frames[0].delay)), None),
            Rate::Auto if delay_gcd(frames) >= 10 => {
                (None, Some(format!("1000/{}", delay_gcd(frames))))
            }
            _ => (None, None),
        })
    }
    pub fn concat(&self, frames: &[UgoiraFrame], duration: bool) -> Result<String> {
        validate_frames(frames)?;
        let mut content = String::from("ffconcat version 1.0\n");
        for frame in frames {
            content.push_str(&format!("file '{}'\n", frame.file));
            if duration {
                let seconds = frame.delay as f64 / 1000.;
                let seconds = if seconds.fract() == 0. {
                    format!("{seconds:.1}")
                } else {
                    seconds.to_string()
                };
                content.push_str(&format!("duration {seconds}\n"));
            }
            if content.len() > MAX_BYTES {
                return Err(error("ugoira concat data exceeds the size limit"));
            }
        }
        if self.repeat {
            content.push_str(&format!(
                "file '{}'\n",
                frames.last().expect("nonempty frames").file
            ));
        }
        Ok(content)
    }
    pub fn timecodes(frames: &[UgoiraFrame]) -> Result<String> {
        validate_frames(frames)?;
        let mut content = String::from("# timecode format v2\n");
        let mut sum = 0u64;
        for frame in frames {
            content.push_str(&format!("{sum}\n"));
            sum = sum
                .checked_add(frame.delay)
                .ok_or_else(|| error("ugoira frame duration overflow"))?;
        }
        content.push_str(&format!("{sum}\n"));
        Ok(content)
    }
    pub fn plan(
        &self,
        source: &[UgoiraFrame],
        directory: &Path,
        output: &Path,
        timestamp: Option<i64>,
    ) -> Result<CommandPlan> {
        validate_frames(source)?;
        let mut frames = source.to_vec();
        let mut concat = None;
        let mut timecodes = None;
        let mut copies = Vec::new();
        let mut mtimes = Vec::new();
        let target = if self.mode == Mode::Mkvmerge {
            directory.join(format!("temp.{}", self.extension))
        } else {
            output.to_owned()
        };
        let mut args = vec![self.ffmpeg.clone().into_os_string()];
        match self.mode {
            Mode::Concat => {
                let (input, output) = self.rates(&frames)?;
                args.extend(["-f".into(), "concat".into()]);
                if let Some(rate) = input {
                    args.extend(["-r".into(), rate.into()]);
                }
                args.extend(["-i".into(), directory.join("ffconcat.txt").into_os_string()]);
                if let Some(rate) = output {
                    args.extend(["-r".into(), rate.into()]);
                }
                concat = Some(self.concat(&frames, true)?);
            }
            Mode::Mkvmerge => {
                args.extend([
                    "-r".into(),
                    "25".into(),
                    "-f".into(),
                    "concat".into(),
                    "-i".into(),
                    directory.join("ffconcat.txt").into_os_string(),
                ]);
                concat = Some(self.concat(&frames, false)?);
                timecodes = Some(Self::timecodes(&frames)?);
            }
            Mode::Image2 => {
                if self.repeat && !uniform(&frames) {
                    let gcd = delay_gcd(&frames);
                    let last = frames.last_mut().expect("nonempty frames");
                    if last.delay > gcd {
                        last.delay -= gcd;
                        let mut extra = last.clone();
                        let (stem, extension) = extra
                            .file
                            .rsplit_once('.')
                            .ok_or_else(|| error("image2 requires numbered frame filenames"))?;
                        let number = stem
                            .trim()
                            .parse::<i128>()
                            .ok()
                            .and_then(|value| value.checked_add(1))
                            .ok_or_else(|| error("image2 requires numbered frame filenames"))?;
                        extra.file = format!("{number:0>6}.{extension}");
                        copies.push((last.file.clone(), extra.file.clone()));
                        frames.push(extra);
                    }
                }
                let mut sum = 0u64;
                for frame in &frames {
                    mtimes.push((frame.file.clone(), sum));
                    sum = sum
                        .checked_add(frame.delay)
                        .ok_or_else(|| error("ugoira frame duration overflow"))?;
                }
                let extension = frames
                    .last()
                    .expect("nonempty frames")
                    .file
                    .rsplit_once('.')
                    .map(|(_, extension)| extension)
                    .unwrap_or("");
                let mut pattern = directory.as_os_str().to_os_string();
                pattern.push(std::path::MAIN_SEPARATOR_STR);
                let pattern = pattern
                    .to_str()
                    .ok_or_else(|| error("image2 temporary path must be valid UTF-8"))?
                    .replace('%', "%%");
                args.extend([
                    "-f".into(),
                    "image2".into(),
                    "-ts_from_file".into(),
                    "2".into(),
                    "-pattern_type".into(),
                    "sequence".into(),
                    "-i".into(),
                    format!("{pattern}%06d.{extension}").into(),
                ]);
            }
            Mode::Archive => {
                return Err(error("archive mode does not invoke conversion processes"));
            }
        }
        if let Some(level) = &self.loglevel {
            args.extend(["-hide_banner".into(), "-loglevel".into(), level.into()]);
        }
        if self.prevent_odd {
            args.extend(["-vf".into(), "crop=iw-mod(iw\\,2):ih-mod(ih\\,2)".into()]);
        }
        args.extend(self.args.clone());
        let mut commands = Vec::new();
        if self.twopass {
            if !self.args.iter().any(|arg| arg == "-f") {
                args.extend(["-f".into(), self.extension.clone().into()]);
            }
            args.extend([
                "-passlogfile".into(),
                directory.join("ffmpeg2pass").into_os_string(),
                "-pass".into(),
            ]);
            let mut first = args.clone();
            first.extend([
                "1".into(),
                "-y".into(),
                if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
            ]);
            commands.push(first);
            args.extend(["2".into(), target.clone().into_os_string()]);
            commands.push(args);
        } else {
            args.push(target.clone().into_os_string());
            commands.push(args);
        }
        if self.mode == Mode::Mkvmerge {
            let mut args = vec![
                self.mkvmerge.clone().into_os_string(),
                "-o".into(),
                output.as_os_str().to_owned(),
                "--timecodes".into(),
            ];
            let mut code = OsString::from("0:");
            code.push(directory.join("timecodes.tc"));
            args.push(code);
            if self.mkvm_mtime
                && let Some(timestamp) = timestamp
            {
                args.extend(["--date".into(), timestamp_string(timestamp)?.into()]);
            }
            if !self.mkvm_metadata {
                args.push("--disable-track-statistics-tags".into());
            }
            if !self.mkvm_output {
                args.push("--quiet".into());
            }
            if self.extension == "webm" {
                args.push("--webm".into());
            }
            args.extend(self.mkvm_args.clone());
            args.extend(["=".into(), target.into_os_string()]);
            commands.push(args);
        }
        Ok(CommandPlan {
            commands,
            frames,
            concat,
            timecodes,
            copies,
            mtimes,
        })
    }
    pub async fn convert(
        &self,
        input: &FrameInput,
        output: &Path,
        media: &Media,
        timeout: Duration,
    ) -> Result<()> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel = Cancel(cancelled.clone());
        tokio::time::timeout(timeout, self.convert_inner(input, output, media, cancelled))
            .await
            .map_err(|_| error("ugoira conversion timed out"))?
    }
    async fn convert_inner(
        &self,
        input: &FrameInput,
        output: &Path,
        media: &Media,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        let timestamp = super::source_timestamp(media);
        if self.mode == Mode::Archive {
            let input = input.clone();
            // Open before the blocking worker starts: cancellation may unlink this
            // staging path, but the worker must never recreate it afterwards.
            let mut output = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(output)
                .map_err(|_| error("could not open converted ugoira archive"))?;
            let metadata = self.metadata.clone();
            let mtime = if self.mtime {
                match &input {
                    FrameInput::Zip { .. } => Some(timestamp.unwrap_or(0)),
                    FrameInput::Files(_) => timestamp,
                }
            } else {
                None
            };
            return tokio::task::spawn_blocking(move || {
                let directory = tempfile::tempdir()
                    .map_err(|_| error("could not create ugoira temporary directory"))?;
                let result = directory.path().join("archive");
                zip::archive(&input, &result, metadata.as_deref(), mtime, &cancelled)?;
                check(&cancelled)?;
                let mut source = std::fs::File::open(result)
                    .map_err(|_| error("could not read converted ugoira archive"))?;
                copy_archive(&mut source, &mut output, &cancelled)?;
                Ok(())
            })
            .await
            .map_err(|_| error("ugoira archive worker failed"))?;
        }
        let input = input.clone();
        let options = self.clone();
        let output_path = output.to_owned();
        let worker_cancel = cancelled.clone();
        let (directory, plan) = tokio::task::spawn_blocking(move || -> Result<_> {
            let directory = tempfile::tempdir()
                .map_err(|_| error("could not create ugoira temporary directory"))?;
            let frames = zip::extract(&input, directory.path(), &worker_cancel)?;
            check(&worker_cancel)?;
            let plan = options.plan(&frames, directory.path(), &output_path, timestamp)?;
            if let Some(content) = &plan.concat {
                std::fs::write(directory.path().join("ffconcat.txt"), content)
                    .map_err(|_| error("could not write ugoira concat data"))?;
            }
            if let Some(content) = &plan.timecodes {
                std::fs::write(directory.path().join("timecodes.tc"), content)
                    .map_err(|_| error("could not write ugoira timecodes"))?;
            }
            for (src, dst) in &plan.copies {
                check(&worker_cancel)?;
                std::fs::copy(directory.path().join(src), directory.path().join(dst))
                    .map_err(|_| error("could not repeat final ugoira frame"))?;
            }
            for (name, milliseconds) in &plan.mtimes {
                check(&worker_cancel)?;
                let time = UNIX_EPOCH
                    .checked_add(Duration::from_millis(*milliseconds))
                    .ok_or_else(|| error("ugoira frame timestamp overflow"))?;
                set_mtime(&directory.path().join(name), time)?;
            }
            Ok((directory, plan))
        })
        .await
        .map_err(|_| error("ugoira preparation worker failed"))??;
        for command in &plan.commands {
            check(&cancelled)?;
            run(command, self.capture).await?;
        }
        let metadata = tokio::fs::metadata(output)
            .await
            .map_err(|_| error("ugoira converter produced no output file"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(error("ugoira converter produced no output file"));
        }
        if self.mtime
            && let Some(timestamp) = timestamp
        {
            let time = if timestamp >= 0 {
                UNIX_EPOCH.checked_add(Duration::from_secs(timestamp as u64))
            } else {
                UNIX_EPOCH.checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
            }
            .ok_or_else(|| error("ugoira output timestamp overflow"))?;
            set_mtime(output, time)?;
        }
        drop(directory);
        Ok(())
    }
}
pub struct CommandPlan {
    pub commands: Vec<Vec<OsString>>,
    pub frames: Vec<UgoiraFrame>,
    pub concat: Option<String>,
    pub timecodes: Option<String>,
    pub copies: Vec<(String, String)>,
    pub mtimes: Vec<(String, u64)>,
}
fn uniform(frames: &[UgoiraFrame]) -> bool {
    frames.iter().all(|frame| frame.delay == frames[0].delay)
}
fn delay_gcd(frames: &[UgoiraFrame]) -> u64 {
    frames.iter().fold(0, |mut left, frame| {
        let mut right = frame.delay;
        while right != 0 {
            let remainder = left % right;
            left = right;
            right = remainder;
        }
        left
    })
}
fn validate_frames(frames: &[UgoiraFrame]) -> Result<()> {
    if frames.is_empty() || frames.len() > MAX_FRAMES {
        return Err(error("ugoira conversion requires 1 to 100000 frames"));
    }
    let mut bytes = 0usize;
    for frame in frames {
        bytes = bytes.saturating_add(frame.file.len());
        if frame.file.is_empty()
            || frame
                .file
                .chars()
                .any(|value| value.is_control() || matches!(value, '\'' | '\\' | ':'))
            || Path::new(&frame.file)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || bytes > MAX_BYTES
            || frame.delay > u32::MAX as u64
        {
            return Err(error("ugoira conversion contains invalid frame metadata"));
        }
    }
    Ok(())
}
fn timestamp_string(timestamp: i64) -> Result<String> {
    let value = time::OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| error("ugoira metadata timestamp is out of range"))?;
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    ))
}
fn set_mtime(path: &Path, time: SystemTime) -> Result<()> {
    super::mtime::open_times_target(path)
        .and_then(|file| {
            file.set_times(
                std::fs::FileTimes::new()
                    .set_accessed(time)
                    .set_modified(time),
            )
        })
        .map_err(|_| error("could not set ugoira file timestamp"))
}
struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn check(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(error("ugoira conversion cancelled"))
    } else {
        Ok(())
    }
}
async fn drain(mut stream: impl tokio::io::AsyncRead + Unpin) -> Result<()> {
    let mut buffer = [0; 8192];
    let mut size = 0usize;
    loop {
        let count = stream
            .read(&mut buffer)
            .await
            .map_err(|_| error("could not read ugoira converter output"))?;
        if count == 0 {
            return Ok(());
        }
        size = size.saturating_add(count);
        if size > MAX_PROCESS_OUTPUT {
            return Err(error("ugoira converter output exceeded the size limit"));
        }
    }
}
async fn run(args: &[OsString], capture: bool) -> Result<()> {
    let (program, args) = args
        .split_first()
        .ok_or_else(|| error("ugoira converter command is empty"))?;
    let output = if capture {
        std::process::Stdio::piped
    } else {
        std::process::Stdio::null
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(output())
        .stderr(output())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| error("could not start ugoira converter"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out = async move {
        if let Some(stdout) = stdout {
            drain(stdout).await
        } else {
            Ok(())
        }
    };
    let err = async move {
        if let Some(stderr) = stderr {
            drain(stderr).await
        } else {
            Ok(())
        }
    };
    let (status, (), ()) = tokio::try_join!(
        async {
            child
                .wait()
                .await
                .map_err(|_| error("could not wait for ugoira converter"))
        },
        out,
        err
    )?;
    if !status.success() {
        return Err(error("ugoira converter exited unsuccessfully"));
    }
    Ok(())
}

fn copy_archive(
    source: &mut impl Read,
    output: &mut impl Write,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut buffer = [0; 8192];
    loop {
        check(cancelled)?;
        let count = source
            .read(&mut buffer)
            .map_err(|_| error("could not read converted ugoira archive"))?;
        if count == 0 {
            break;
        }
        check(cancelled)?;
        output
            .write_all(&buffer[..count])
            .map_err(|_| error("could not write converted ugoira archive"))?;
    }
    check(cancelled)?;
    output
        .flush()
        .map_err(|_| error("could not flush converted ugoira archive"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cancelled_archive_copy_cannot_recreate_unlinked_staging_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stage");
        let mut handle = std::fs::File::create(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let cancelled = AtomicBool::new(true);
        assert!(copy_archive(&mut &b"archive"[..], &mut handle, &cancelled).is_err());
        assert!(!path.exists());
        let mut target = Vec::new();
        cancelled.store(false, Ordering::Relaxed);
        copy_archive(&mut &b"archive"[..], &mut target, &cancelled).unwrap();
        assert_eq!(target, b"archive");
    }
    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/pixiv/ugoira_postprocessor.json"
        ))
        .unwrap()
    }
    fn options(value: &Value) -> UgoiraOptions {
        UgoiraOptions::parse_detected(value.as_object().unwrap(), Path::new("/fixture"), false)
            .unwrap()
    }
    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect()
    }
    #[test]
    fn source_option_vectors_cover_defaults_aliases_truthiness_codec_order_and_auto_detection() {
        for case in fixture()["option_cases"].as_array().unwrap() {
            let native = UgoiraOptions::parse_detected(
                case["options"].as_object().unwrap(),
                Path::new("/fixture"),
                case["mkvmerge_detected"] == true,
            )
            .unwrap();
            let expected = &case["normalized"];
            assert_eq!(
                native.extension,
                expected["extension"].as_str().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                strings(&native.args),
                expected["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value.as_str().unwrap())
                    .collect::<Vec<_>>()
            );
            assert_eq!(native.keep, !expected["delete"].as_bool().unwrap());
            assert_eq!(native.skip, expected["skip"].as_bool().unwrap());
            assert_eq!(native.repeat, truth(&expected["repeat"]));
            assert_eq!(native.twopass, truth(&expected["twopass"]));
            assert_eq!(native.mtime, truth(&expected["mtime"]));
            assert_eq!(
                native.prevent_odd,
                expected["prevent_odd"].as_bool().unwrap()
            );
            assert_eq!(native.mkvm_mtime, truth(&expected["mkvmerge_mtime"]));
            assert_eq!(native.mkvm_output, truth(&expected["mkvmerge_output"]));
            assert_eq!(native.mkvm_metadata, truth(&expected["mkvmerge_metadata"]));
            let expected_mode = if expected["convert_impl"] == "convert_to_archive" {
                Mode::Archive
            } else {
                match expected["process"].as_str().unwrap() {
                    "_process_image2" => Mode::Image2,
                    "_process_mkvmerge" => Mode::Mkvmerge,
                    _ => Mode::Concat,
                }
            };
            assert_eq!(native.mode, expected_mode, "{}", case["name"]);
        }
    }
    #[test]
    fn source_rate_vectors_cover_uniform_gcd_zero_custom_and_empty_frames() {
        for case in fixture()["rate_cases"].as_array().unwrap() {
            let frames: Vec<UgoiraFrame> = serde_json::from_value(case["frames"].clone()).unwrap();
            let result = options(&case["options"]).rates(&frames);
            if case["error"].is_null() {
                let expected = case["rate"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| {
                        if !truth(value) {
                            Value::Null
                        } else {
                            Value::String(match value {
                                Value::String(value) => value.clone(),
                                Value::Bool(value) => {
                                    if *value {
                                        "True".into()
                                    } else {
                                        "False".into()
                                    }
                                }
                                _ => value.to_string(),
                            })
                        }
                    })
                    .collect::<Vec<_>>();
                assert_eq!(json!(result.unwrap()), json!(expected), "{}", case["name"]);
            } else {
                assert!(result.is_err(), "{}", case["name"]);
            }
        }
    }
    #[test]
    fn source_command_vectors_match_concat_image2_twopass_mkvmerge_and_frame_timestamps() {
        for case in fixture()["command_cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| case["name"] != "existing_output_skips")
        {
            let native = options(&case["options"]);
            let frames: Vec<UgoiraFrame> = serde_json::from_value(case["frames"].clone()).unwrap();
            let directory = Path::new("$WORK/frames");
            let output = PathBuf::from(format!("$WORK/out/42_p0.{}", native.extension()));
            let plan = native
                .plan(
                    &frames,
                    directory,
                    &output,
                    crate::date::parse_date("2024-01-02T03:04:06+00:00").ok(),
                )
                .unwrap();
            let expected: Vec<Vec<String>> = case["calls"]
                .as_array()
                .unwrap()
                .iter()
                .map(|call| {
                    call["args"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect()
                })
                .collect();
            assert_eq!(
                plan.commands
                    .iter()
                    .map(|args| {
                        // Joined paths and the null device are native, as in
                        // gallery-dl; the oracle was recorded on POSIX.
                        strings(args)
                            .into_iter()
                            .map(|arg| {
                                if !cfg!(windows) {
                                    arg
                                } else if arg == "NUL" {
                                    "/dev/null".to_owned()
                                } else if arg.contains("$WORK") {
                                    arg.replace('\\', "/")
                                } else {
                                    arg
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>(),
                expected,
                "{}",
                case["name"]
            );
            assert_eq!(
                plan.concat.as_deref(),
                case["text_files"]
                    .get("ffconcat.txt")
                    .and_then(Value::as_str),
                "{}",
                case["name"]
            );
            assert_eq!(
                plan.timecodes.as_deref(),
                case["text_files"]
                    .get("timecodes.tc")
                    .and_then(Value::as_str),
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::to_value(&plan.frames).unwrap(),
                case["mutated_frames"],
                "{}",
                case["name"]
            );
            let actual:Vec<_>=plan.mtimes.iter().map(|(name,milliseconds)|json!({"path":directory.join(name).to_str().unwrap().replace(std::path::MAIN_SEPARATOR, "/"),"ns":[milliseconds*1_000_000,milliseconds*1_000_000]})).collect();
            assert_eq!(json!(actual), case["utimes"], "{}", case["name"]);
        }
    }
    #[test]
    fn unsafe_concat_names_invalid_arguments_and_unbounded_options_fail_privately() {
        for file in [
            "../private.jpg",
            "/private.jpg",
            "a'b.jpg",
            "http:private.jpg",
            "a\\b.jpg",
            "a\nb.jpg",
        ] {
            let frames = vec![UgoiraFrame {
                file: file.into(),
                delay: 50,
            }];
            let error = options(&json!({}))
                .concat(&frames, true)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private"));
        }
        for value in [
            json!({"ffmpeg-args":["-c:v"]}),
            json!({"ffmpeg-args":[null]}),
            json!({"extension":"../private"}),
            json!({"ffmpeg-location":"private\u{0000}"}),
        ] {
            let error = UgoiraOptions::parse(value.as_object().unwrap(), Path::new("."))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private"));
        }
        assert_eq!(
            expanded("${GALLERY_DL_UNDEFINED_FIXTURE"),
            "${GALLERY_DL_UNDEFINED_FIXTURE"
        );
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use crate::{
        extract::{MediaJob, PostMetadata},
        postprocess::SourceFrame,
    };
    use serde_json::json;
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};
    fn script(directory: &Path, name: &str, content: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{content}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn source(directory: &Path) -> FrameInput {
        let mut frames = Vec::new();
        for (index, delay) in [50, 125].into_iter().enumerate() {
            let path = directory.join(format!("source{index}.jpg"));
            std::fs::write(&path, b"FRAME_SOURCE_SENTINEL").unwrap();
            frames.push(SourceFrame {
                frame: UgoiraFrame {
                    file: format!("{index:06}.jpg"),
                    delay,
                },
                path,
                extension: "jpg".into(),
            });
        }
        FrameInput::Files(frames)
    }
    fn media() -> Media {
        Media {
            url: url::Url::parse(
                "https://i.pximg.net/img-original/img/2024/01/02/03/04/05/42_ugoira0.jpg",
            )
            .unwrap(),
            job: MediaJob::DirectHttp,
            category: "pixiv".into(),
            filename: "42_p0.jpg".into(),
            metadata: Some(PostMetadata::default()),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        }
    }
    fn options(value: Value) -> UgoiraOptions {
        UgoiraOptions::parse(value.as_object().unwrap(), Path::new(".")).unwrap()
    }
    #[tokio::test]
    async fn process_pipeline_handles_all_strategies_twopass_and_preserves_sources() {
        let directory = tempfile::tempdir().unwrap();
        let input = source(directory.path());
        let ffmpeg = script(
            directory.path(),
            "ffmpeg",
            "for argument do target=\"$argument\"; done\nprintf 'CONVERTED' > \"$target\"",
        );
        let mkvmerge = script(
            directory.path(),
            "mkvmerge",
            "previous=''\nfor argument do\n if [ \"$previous\" = '-o' ]; then target=\"$argument\"; fi\n previous=\"$argument\"\ndone\nprintf 'REMUXED' > \"$target\"",
        );
        for (mode, twopass) in [
            ("concat", false),
            ("concat", true),
            ("image2", false),
            ("mkvmerge", false),
        ] {
            let output = directory.path().join(format!("{mode}-{twopass}.webm"));
            options(json!({"mode":mode,"ffmpeg-location":ffmpeg,"mkvmerge-location":mkvmerge,"ffmpeg-twopass":twopass,"mtime":false})).convert(&input,&output,&media(),Duration::from_secs(5)).await.unwrap();
            assert_eq!(
                std::fs::read(&output).unwrap(),
                if mode == "mkvmerge" {
                    b"REMUXED".as_slice()
                } else {
                    b"CONVERTED".as_slice()
                }
            );
        }
        if let FrameInput::Files(frames) = &input {
            for frame in frames {
                assert_eq!(
                    std::fs::read(&frame.path).unwrap(),
                    b"FRAME_SOURCE_SENTINEL"
                );
            }
        }
    }
    #[tokio::test]
    async fn converter_failures_and_output_flood_are_private_and_do_not_delete_source_files() {
        let directory = tempfile::tempdir().unwrap();
        let input = source(directory.path());
        for (name, content, needle) in [
            (
                "failed",
                "printf 'PRIVATE_ERROR_TOKEN' >&2\nexit 7",
                "unsuccessfully",
            ),
            (
                "flood",
                "while :; do printf '0123456789012345678901234567890123456789012345678901234567890123456789'; done",
                "size limit",
            ),
        ] {
            let program = script(directory.path(), name, content);
            let output = directory.path().join(format!("{name}.webm"));
            let error = options(json!({"mode":"concat","ffmpeg-location":program}))
                .convert(&input, &output, &media(), Duration::from_secs(5))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(needle), "{error}");
            assert!(!error.contains("PRIVATE"));
            assert!(!error.contains(directory.path().to_str().unwrap()));
            assert!(!output.exists());
        }
        if let FrameInput::Files(frames) = &input {
            assert!(frames.iter().all(|frame| frame.path.exists()));
        }
    }
    #[tokio::test]
    async fn timeout_kills_the_running_converter_and_leaves_the_input_group_intact() {
        let directory = tempfile::tempdir().unwrap();
        let input = source(directory.path());
        let pidfile = directory.path().join("pid");
        let quoted = pidfile.to_str().unwrap().replace('\'', "'\\''");
        let program = script(
            directory.path(),
            "slow",
            &format!("printf '%s' \"$$\" > '{quoted}'\nexec /bin/sleep 30"),
        );
        let output = directory.path().join("failed.webm");
        let worker_input = input.clone();
        let worker_program = program.clone();
        let worker_output = output.clone();
        let worker = tokio::spawn(async move {
            options(json!({"mode":"concat","ffmpeg-location":worker_program}))
                .convert(
                    &worker_input,
                    &worker_output,
                    &media(),
                    Duration::from_secs(30),
                )
                .await
        });
        let startup = tokio::time::Instant::now() + Duration::from_secs(10);
        while !pidfile.exists() {
            assert!(
                tokio::time::Instant::now() < startup,
                "converter did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let pid = std::fs::read_to_string(pidfile).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = tokio::process::Command::new("/bin/kill")
                .args(["-0", pid.as_str()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .unwrap();
            if !status.success() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "cancelled converter remained running"
            );
            tokio::task::yield_now().await;
        }
        // A spawn failure (e.g. no ffmpeg on PATH) would beat a zero deadline.
        let error = options(json!({"mode":"concat","ffmpeg-location":program}))
            .convert(&input, &output, &media(), Duration::ZERO)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"));
        if let FrameInput::Files(frames) = &input {
            assert!(frames.iter().all(|frame| frame.path.exists()));
        }
        assert!(!output.exists());
    }
}
