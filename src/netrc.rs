//! Minimal `.netrc` reader for upstream's `netrc` option: credentials are
//! looked up by extractor category as the machine name.
use std::path::PathBuf;

/// `(login, password)` for `machine`, falling back to a `default` entry.
pub fn lookup(text: &str, machine: &str) -> Option<(String, String)> {
    // (machine, or None for `default`; login; password)
    let mut entries: Vec<(Option<String>, String, String)> = Vec::new();
    let mut tokens = Tokens::new(text);
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "machine" => entries.push((
                Some(tokens.next().unwrap_or_default()),
                "".into(),
                "".into(),
            )),
            "default" => entries.push((None, String::new(), String::new())),
            "login" | "user" => {
                let value = tokens.next().unwrap_or_default();
                if let Some(entry) = entries.last_mut() {
                    entry.1 = value;
                }
            }
            "password" => {
                let value = tokens.next().unwrap_or_default();
                if let Some(entry) = entries.last_mut() {
                    entry.2 = value;
                }
            }
            "account" => {
                tokens.next();
            }
            "macdef" => {
                tokens.next();
                tokens.skip_macro();
            }
            _ => {}
        }
    }
    let find = |wanted: Option<&str>| {
        entries
            .iter()
            .find(|(name, _, _)| name.as_deref() == wanted)
            .map(|(_, login, password)| (login.clone(), password.clone()))
    };
    find(Some(machine)).or_else(|| find(None))
}

/// The default `.netrc` location (`$NETRC`, then `~/.netrc`).
pub fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NETRC") {
        return Some(path.into());
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".netrc"))
}

struct Tokens<'a> {
    rest: &'a str,
}
impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text }
    }
    fn skip_macro(&mut self) {
        // A macro definition ends at the first empty line.
        match self.rest.find("\n\n") {
            Some(end) => self.rest = &self.rest[end + 2..],
            None => self.rest = "",
        }
    }
    fn next(&mut self) -> Option<String> {
        loop {
            self.rest = self.rest.trim_start();
            if let Some(comment) = self.rest.strip_prefix('#') {
                self.rest = comment.split_once('\n').map_or("", |(_, rest)| rest);
                continue;
            }
            break;
        }
        if self.rest.is_empty() {
            return None;
        }
        let mut token = String::new();
        if let Some(quoted) = self.rest.strip_prefix('"') {
            let mut chars = quoted.char_indices();
            let mut end = quoted.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    '\\' => {
                        if let Some((_, escaped)) = chars.next() {
                            token.push(escaped);
                        }
                    }
                    c => token.push(c),
                }
            }
            self.rest = &quoted[end.min(quoted.len())..];
        } else {
            let end = self
                .rest
                .find(char::is_whitespace)
                .unwrap_or(self.rest.len());
            token.push_str(&self.rest[..end]);
            self.rest = &self.rest[end..];
        }
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_machine_entries_defaults_and_skips_macros() {
        let text = "# comment\nmachine pixiv login alice password \"p w\"\n\
                    macdef init\ncd /\nmachine fake login x\n\n\
                    machine twitter\n  login bob\n  password secret\n\
                    default login anon password guest\n";
        assert_eq!(lookup(text, "pixiv"), Some(("alice".into(), "p w".into())));
        assert_eq!(
            lookup(text, "twitter"),
            Some(("bob".into(), "secret".into()))
        );
        assert_eq!(lookup(text, "fake"), Some(("anon".into(), "guest".into())));
        assert_eq!(lookup("machine a login b", "c"), None);
    }
}
