//! Upstream-style `--exec` postprocessing: `{}` is replaced by the quoted
//! file path; commands run through the platform shell.
use crate::{Result, error};
use std::path::Path;

pub fn quote(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Substitute `{}` with the quoted path, like upstream's string commands.
pub fn render(command: &str, path: Option<&Path>) -> String {
    match path {
        Some(path) => command.replace("{}", &quote(&path.to_string_lossy())),
        None => command.to_owned(),
    }
}

pub async fn run(command: &str, path: Option<&Path>) -> Result<()> {
    let command = render(command, path);
    let mut process = if cfg!(windows) {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C").arg(&command);
        process
    } else {
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(&command);
        process
    };
    let status = process.kill_on_drop(true).status().await?;
    if status.success() {
        Ok(())
    } else {
        Err(error(format!("exec command exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn paths_are_shell_quoted() {
        assert_eq!(
            render("echo {} {}", Some(Path::new("a b/it's.jpg"))),
            "echo 'a b/it'\\''s.jpg' 'a b/it'\\''s.jpg'"
        );
        assert_eq!(render("true", None), "true");
    }

    #[cfg(windows)]
    #[test]
    fn paths_are_cmd_quoted() {
        assert_eq!(
            render("echo {}", Some(Path::new(r#"a b\say "hi".jpg"#))),
            r#"echo "a b\say ""hi"".jpg""#
        );
        assert_eq!(render("true", None), "true");
    }
}
