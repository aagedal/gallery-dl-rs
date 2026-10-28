//! Upstream `output.mode`: how completed and skipped files are reported on
//! standard output.
//!
//! `auto` (default) uses colors on a terminal and plain paths otherwise;
//! `pipe`/`default` print `PATH` and `# PATH`; `terminal` prints `✔ PATH`
//! and `# PATH`; `color` colors the path (`output.colors`, `NO_COLOR`);
//! `null`/`false` print nothing; an object gives `skip`/`success` format
//! strings with `{}` for the path. `output.skip: false` hides skips.
//! Paths are not shortened to the terminal width.
use serde_json::Value;
use std::io::{IsTerminal, Write};

#[derive(Clone, Debug)]
enum Mode {
    Null,
    Pipe,
    Terminal,
    Color {
        success: String,
        skip: String,
    },
    Custom {
        success: Option<String>,
        skip: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct Output {
    mode: Mode,
    skip: bool,
}

impl Output {
    /// Build from the `output` configuration section.
    pub fn new(section: Option<&serde_json::Map<String, Value>>, quiet: bool) -> Self {
        let get = |key: &str| section.and_then(|s| s.get(key));
        let skip = get("skip") != Some(&Value::Bool(false));
        let colors = |default_success: &str, default_skip: &str| {
            let colors = get("colors").and_then(Value::as_object);
            let color = |key: &str, default: &str| {
                colors
                    .and_then(|c| c.get(key))
                    .and_then(Value::as_str)
                    .unwrap_or(default)
                    .to_owned()
            };
            Mode::Color {
                success: color("success", default_success),
                skip: color("skip", default_skip),
            }
        };
        let tty = std::io::stdout().is_terminal();
        let mode = if quiet {
            Mode::Null
        } else {
            match get("mode") {
                None | Some(Value::Null) => Self::auto(tty, colors),
                Some(Value::Bool(false)) => Mode::Null,
                Some(Value::Object(custom)) => {
                    let format = |key: &str| {
                        custom.get(key).and_then(|v| match v {
                            Value::String(s) => Some(s.clone()),
                            // `[offset, format]`
                            Value::Array(pair) => pair.get(1)?.as_str().map(str::to_owned),
                            _ => None,
                        })
                    };
                    Mode::Custom {
                        success: format("success"),
                        skip: format("skip"),
                    }
                }
                Some(Value::String(name)) => match name.to_ascii_lowercase().as_str() {
                    "auto" => Self::auto(tty, colors),
                    "pipe" | "default" => Mode::Pipe,
                    "term" | "terminal" => Mode::Terminal,
                    "color" => colors("1;32", "2"),
                    _ => Mode::Null,
                },
                Some(_) => Mode::Pipe,
            }
        };
        Self { mode, skip }
    }

    fn auto(tty: bool, colors: impl Fn(&str, &str) -> Mode) -> Mode {
        if !tty {
            Mode::Pipe
        } else if std::env::var_os("NO_COLOR").is_some() {
            Mode::Terminal
        } else {
            colors("1;32", "2")
        }
    }

    fn write(text: String) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }

    /// A downloaded file.
    pub fn success(&self, path: &str) {
        let text = match &self.mode {
            Mode::Null => return,
            Mode::Pipe => format!("{path}\n"),
            Mode::Terminal => format!("✔ {path}\n"),
            Mode::Color { success, .. } => format!("\x1b[{success}m{path}\x1b[0m\n"),
            Mode::Custom { success, .. } => match success {
                Some(format) => format.replace("{}", path),
                None => return,
            },
        };
        Self::write(text);
    }

    /// A skipped (existing) file.
    pub fn skip(&self, path: &str) {
        if !self.skip {
            return;
        }
        let text = match &self.mode {
            Mode::Null => return,
            Mode::Pipe | Mode::Terminal => format!("# {path}\n"),
            Mode::Color { skip, .. } => format!("\x1b[{skip}m{path}\x1b[0m\n"),
            Mode::Custom { skip, .. } => match skip {
                Some(format) => format.replace("{}", path),
                None => return,
            },
        };
        Self::write(text);
    }
}
