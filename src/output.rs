//! Upstream `output.mode`: how completed and skipped files are reported on
//! standard output.
//!
//! `auto` (default) uses colors on a terminal and plain paths otherwise;
//! `pipe`/`default` print `PATH` and `# PATH`; `terminal` prints `✔ PATH`
//! and `# PATH`; `color` colors the path (`output.colors`, `NO_COLOR`);
//! `null`/`false` print nothing; an object gives `skip`/`success` format
//! strings with `{}` for the path. `output.skip: false` hides skips.
//! Terminal and color modes shorten paths to the terminal width
//! (`output.shorten`, default true).
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
    /// Upstream `output.shorten`: terminal width for terminal/color modes.
    width: Option<usize>,
}

/// Columns of the terminal on standard output.
fn terminal_width() -> Option<usize> {
    #[cfg(unix)]
    {
        // SAFETY: TIOCGWINSZ fills a plain winsize struct.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 {
                return Some(size.ws_col as usize);
            }
        }
    }
    std::env::var("COLUMNS").ok()?.parse().ok()
}

/// Upstream `shorten_string`: keep both ends around an ellipsis.
fn shorten(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit || limit < 2 {
        return text.to_owned();
    }
    let limit = limit - 1;
    let head: String = chars[..limit / 2].iter().collect();
    let tail: String = chars[chars.len() - limit.div_ceil(2)..].iter().collect();
    format!("{head}…{tail}")
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
        let width = (matches!(mode, Mode::Terminal | Mode::Color { .. })
            && get("shorten") != Some(&Value::Bool(false)))
        .then(terminal_width)
        .flatten();
        Self { mode, skip, width }
    }

    fn fit(&self, text: &str) -> String {
        match self.width {
            Some(width) => shorten(text, width.saturating_sub(1)),
            None => text.to_owned(),
        }
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
            Mode::Terminal => format!("{}\n", self.fit(&format!("✔ {path}"))),
            Mode::Color { success, .. } => {
                format!("\x1b[{success}m{}\x1b[0m\n", self.fit(path))
            }
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
            Mode::Pipe => format!("# {path}\n"),
            Mode::Terminal => format!("{}\n", self.fit(&format!("# {path}"))),
            Mode::Color { skip, .. } => format!("\x1b[{skip}m{}\x1b[0m\n", self.fit(path)),
            Mode::Custom { skip, .. } => match skip {
                Some(format) => format.replace("{}", path),
                None => return,
            },
        };
        Self::write(text);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shortens_like_upstream() {
        assert_eq!(super::shorten("abcdefghij", 20), "abcdefghij");
        assert_eq!(super::shorten("abcdefghij", 6), "ab…hij");
    }
}
