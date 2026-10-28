//! Upstream `actions`: small commands run on events such as exec
//! postprocessor results (`success`/`error`) or matching log messages.
//!
//! Supported: `print`, `status`, `level`, `exec`, `wait`, `keyword`,
//! `abort`, `terminate`, `restart` (treated as `abort`), `exit` and
//! `raise` (fails the current operation with the given message).
use crate::{Result, Stop, error};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

/// One parsed action.
#[derive(Clone, Debug)]
enum Action {
    Print(String),
    Status(char, i32),
    Level(String),
    Exec(String),
    Wait(Option<crate::sleep::SleepRange>),
    Keyword(String, Value),
    Raise(String),
    Stop(Stop),
}

/// A chain of actions, run in order.
#[derive(Clone, Debug, Default)]
pub struct Actions(Vec<Action>);

/// Exit status changes made by `status` actions: the operator (`&`, `|`,
/// `^`, `=`) applied last for each bit is replayed on the final status.
static STATUS_OPS: std::sync::Mutex<Vec<(char, i32)>> = std::sync::Mutex::new(Vec::new());
/// Log level overrides set by `level` actions while a message is handled.
static LEVEL: AtomicU32 = AtomicU32::new(0);
/// Exit code requested by an `exit` action, if any.
static EXIT: AtomicI32 = AtomicI32::new(i32::MIN);

fn parse_one(spec: &str) -> Result<Action> {
    let (kind, args) = spec.split_once(' ').unwrap_or((spec, ""));
    Ok(match kind {
        "print" => Action::Print(args.to_owned()),
        "status" => {
            let args = args.trim_start();
            let op = args
                .chars()
                .next()
                .filter(|c| "&|^=".contains(*c))
                .ok_or_else(|| error(format!("invalid status action '{args}'")))?;
            let value = args[1..]
                .trim_start_matches('=')
                .trim()
                .parse()
                .map_err(|_| error(format!("invalid status action '{args}'")))?;
            Action::Status(op, value)
        }
        "level" => Action::Level(args.trim_start_matches([' ', '~', '=']).to_owned()),
        "exec" => Action::Exec(args.to_owned()),
        "wait" => Action::Wait(if args.trim().is_empty() {
            None
        } else {
            crate::sleep::SleepRange::from_value(&Value::String(args.trim().to_owned()))?
        }),
        "keyword" => {
            let (name, value) = args.split_once(' ').unwrap_or((args, ""));
            let value = value.trim();
            let value = serde_json::from_str(value).unwrap_or_else(|_| value.into());
            Action::Keyword(name.to_owned(), value)
        }
        "raise" => Action::Raise(args.to_owned()),
        "abort" | "restart" => Action::Stop(Stop::Abort),
        "terminate" => Action::Stop(Stop::Terminate),
        "exit" => Action::Stop(Stop::Exit(args.trim().parse().unwrap_or(0))),
        "flag" => return Err(error("the 'flag' action is not supported natively")),
        _ => return Err(error(format!("unknown action '{kind}'"))),
    })
}

impl Actions {
    /// Parse an action string or a list of action strings. Like upstream,
    /// actions that change state (`status`, `level`, ...) run before
    /// actions with effects (`print`, `exec`, `abort`, ...).
    pub fn parse(spec: &Value) -> Result<Self> {
        let specs = match spec {
            Value::String(spec) => vec![spec.as_str()],
            Value::Array(specs) => specs
                .iter()
                .map(|spec| {
                    spec.as_str()
                        .ok_or_else(|| error("actions must be strings"))
                })
                .collect::<Result<_>>()?,
            _ => return Err(error("actions must be a string or a list")),
        };
        let mut actions = specs
            .into_iter()
            .map(parse_one)
            .collect::<Result<Vec<_>>>()?;
        actions.sort_by_key(|action| !action.before());
        Ok(Self(actions))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Run the actions; stop actions end in a [`Stop`] error.
    pub async fn run(&self, keywords: Option<&mut Map<String, Value>>) -> Result<()> {
        let actions = self.clone();
        let mut owned = keywords.as_ref().map(|k| (*k).clone());
        let (result, owned) = tokio::task::spawn_blocking(move || {
            let result = actions.run_blocking(owned.as_mut());
            (result, owned)
        })
        .await?;
        if let (Some(keywords), Some(owned)) = (keywords, owned) {
            *keywords = owned;
        }
        result
    }

    /// [`Actions::run`] on the current thread (upstream runs actions
    /// synchronously too).
    pub fn run_blocking(&self, mut keywords: Option<&mut Map<String, Value>>) -> Result<()> {
        for action in &self.0 {
            match action {
                Action::Print(text) => println!("{text}"),
                Action::Status(op, value) => STATUS_OPS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((*op, *value)),
                Action::Level(level) => LEVEL.store(level_number(level), Ordering::Relaxed),
                Action::Exec(command) => {
                    let mut process = if cfg!(windows) {
                        let mut process = std::process::Command::new("cmd");
                        process.arg("/C");
                        process
                    } else {
                        let mut process = std::process::Command::new("sh");
                        process.arg("-c");
                        process
                    };
                    process.arg(command).status()?;
                }
                Action::Wait(Some(range)) => std::thread::sleep(range.sample()),
                Action::Wait(None) => {
                    eprint!("Press Enter to continue");
                    std::io::stdin().read_line(&mut String::new())?;
                }
                Action::Keyword(name, value) => {
                    if let Some(keywords) = keywords.as_deref_mut() {
                        keywords.insert(name.clone(), value.clone());
                    }
                }
                Action::Raise(message) => {
                    return Err(error(if message.is_empty() {
                        "action raised an error".to_owned()
                    } else {
                        message.clone()
                    }));
                }
                Action::Stop(stop) => {
                    if let Stop::Exit(code) = stop {
                        EXIT.store(*code, Ordering::Relaxed);
                    }
                    return Err((*stop).into());
                }
            }
        }
        Ok(())
    }
}

impl Action {
    fn before(&self) -> bool {
        matches!(self, Self::Status(..) | Self::Level(_) | Self::Keyword(..))
    }
}

fn level_number(level: &str) -> u32 {
    match level.trim().to_ascii_uppercase().as_str() {
        "DEBUG" => 10,
        "INFO" => 20,
        "WARNING" | "WARN" => 30,
        "ERROR" => 40,
        "CRITICAL" | "FATAL" => 50,
        other => other.parse().unwrap_or(0),
    }
}

/// Upstream job status bits of failed operations.
static STATUS: AtomicI32 = AtomicI32::new(0);

/// Upstream exception codes: 4 extraction/HTTP errors, 8 challenges,
/// 16 authentication/authorization, 32 input/format errors, 64 unsupported
/// URLs, 128 local OS errors.
pub mod code {
    pub const ERROR: i32 = 1;
    pub const HTTP: i32 = 4;
    pub const CHALLENGE: i32 = 8;
    pub const AUTH: i32 = 16;
    pub const FORMAT: i32 = 32;
    pub const NO_EXTRACTOR: i32 = 64;
    pub const OS: i32 = 128;
}

/// Record a failure for the exit status.
pub fn fail(bits: i32) {
    STATUS.fetch_or(bits, Ordering::Relaxed);
}

/// The upstream status code of an extraction error, from its message.
pub fn classify(message: &str) -> i32 {
    let lower = message.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if has(&["unsupported url", "no extractor"]) {
        code::NO_EXTRACTOR
    } else if has(&["cloudflare", "challenge", "captcha"]) {
        code::CHALLENGE
    } else if has(&[
        "credential",
        "login",
        "log in",
        "authenticat",
        "authoriz",
        "401 unauthorized",
        "api key",
        "api-key",
        "access token",
    ]) {
        code::AUTH
    } else if has(&["template", "format string", "filename field"]) {
        code::FORMAT
    } else {
        code::HTTP
    }
}

/// The final exit status: the recorded failure bits (or `base` if none
/// were recorded) modified by `status` actions, or the code of an `exit`
/// action.
pub fn exit_status(base: i32) -> i32 {
    let exit = EXIT.load(Ordering::Relaxed);
    if exit != i32::MIN {
        return exit;
    }
    let base = match STATUS.load(Ordering::Relaxed) {
        0 => base,
        bits => bits,
    };
    STATUS_OPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .fold(base, |status, (op, value)| match op {
            '&' => status & value,
            '|' => status | value,
            '^' => status ^ value,
            _ => *value,
        })
}

/// Upstream logging actions (`extractor.*.actions`): actions keyed by
/// `LEVEL[:REGEX]`, run for log messages of that level matching REGEX.
#[derive(Clone, Debug, Default)]
pub struct LogActions(Vec<(u32, Option<regex::Regex>, Actions)>);

impl LogActions {
    pub fn parse(spec: &Value) -> Result<Self> {
        let entries: Vec<(String, Value)> = match spec {
            Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Value::Array(pairs) => pairs
                .iter()
                .map(|pair| match pair.as_array().map(Vec::as_slice) {
                    Some([Value::String(event), spec]) => Ok((event.clone(), spec.clone())),
                    _ => Err(error("actions must be [event, action] pairs")),
                })
                .collect::<Result<_>>()?,
            _ => return Err(error("actions must be an object or a list")),
        };
        entries
            .into_iter()
            .map(|(event, spec)| {
                let (level, pattern) = event.split_once(':').unwrap_or((&event, ""));
                let level = match level.trim() {
                    "" | "*" => 0,
                    level => level_number(level),
                };
                let pattern = (!pattern.is_empty())
                    .then(|| regex::Regex::new(pattern))
                    .transpose()
                    .map_err(|e| error(format!("invalid action pattern: {e}")))?;
                Ok((level, pattern, Actions::parse(&spec)?))
            })
            .collect::<Result<_>>()
            .map(Self)
    }

    /// Run the actions matching a message of `level`. Returns the level to
    /// log the message at (changed by `level` actions).
    pub fn run(&self, level: u32, message: &str) -> Result<u32> {
        LEVEL.store(level, Ordering::Relaxed);
        for (wanted, pattern, actions) in &self.0 {
            if (*wanted == 0 || *wanted == level)
                && pattern.as_ref().is_none_or(|p| p.is_match(message))
            {
                actions.run_blocking(None)?;
            }
        }
        Ok(LEVEL.load(Ordering::Relaxed))
    }
}

static LOG_ACTIONS: std::sync::OnceLock<LogActions> = std::sync::OnceLock::new();

/// Install the process-wide logging actions (upstream `extractor.actions`).
pub fn install(actions: LogActions) {
    let _ = LOG_ACTIONS.set(actions);
}

/// Report a diagnostic at `level` (see [`WARNING`], [`ERROR`]): run matching
/// logging actions, then print `message` unless a `level` action lowered it
/// below warnings. Stop actions are returned as errors.
pub fn log(level: u32, message: &str) -> Result<()> {
    log_as("gallery-dl", level, message)
}

static LOG_FORMAT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Install upstream `output.log` (a format string or `{"format": ...}`).
pub fn set_log_format(value: &Value) {
    let format = match value {
        Value::String(format) => Some(format.clone()),
        Value::Object(map) => map.get("format").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    };
    if let Some(format) = format {
        let _ = LOG_FORMAT.set(format);
    }
}

fn level_name(level: u32) -> &'static str {
    match level {
        0..=10 => "debug",
        11..=20 => "info",
        21..=30 => "warning",
        31..=40 => "error",
        _ => "critical",
    }
}

/// [`log`] from the logger `name` (upstream's `[NAME][LEVEL] MESSAGE`
/// format, configurable with `output.log`).
pub fn log_as(name: &str, level: u32, message: &str) -> Result<()> {
    let level = match LOG_ACTIONS.get() {
        Some(actions) => actions.run(level, message)?,
        None => level,
    };
    if level >= WARNING {
        let format = LOG_FORMAT
            .get()
            .map(String::as_str)
            .unwrap_or("[{name}][{levelname}] {message}");
        let now = time::OffsetDateTime::now_utc();
        let asctime = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            now.year(),
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
        eprintln!(
            "{}",
            format
                .replace("{name}", name)
                .replace("{levelname}", level_name(level))
                .replace("{asctime}", &asctime)
                .replace("{message}", message)
        );
    }
    Ok(())
}

pub const WARNING: u32 = 30;
pub const ERROR: u32 = 40;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_errors_like_upstream_exceptions() {
        assert_eq!(
            classify("unsupported URL; expected ..."),
            code::NO_EXTRACTOR
        );
        assert_eq!(classify("login required"), code::AUTH);
        assert_eq!(classify("HTTP 404 Not Found"), code::HTTP);
        assert_eq!(classify("Cloudflare challenge"), code::CHALLENGE);
    }

    #[tokio::test]
    async fn parses_and_runs_actions() {
        let actions = Actions::parse(&json!(["abort", "keyword foo [1, 2]"])).unwrap();
        let mut keywords = Map::new();
        let result = actions.run(Some(&mut keywords)).await;
        assert_eq!(Stop::of(&result.unwrap_err()), Some(Stop::Abort));
        assert_eq!(keywords["foo"], json!([1, 2]));
        assert!(Actions::parse(&json!("bogus")).is_err());
        assert!(Actions::parse(&json!("status |= 4")).is_ok());

        let log = LogActions::parse(&json!({"warning:^skip": "level error"})).unwrap();
        assert_eq!(log.run(WARNING, "skipping x").unwrap(), ERROR);
        assert_eq!(log.run(WARNING, "other").unwrap(), WARNING);
        let terminate = LogActions::parse(&json!({"error": ["print x", "terminate"]})).unwrap();
        assert_eq!(
            Stop::of(&terminate.run(ERROR, "boom").unwrap_err()),
            Some(Stop::Terminate)
        );
    }
}
