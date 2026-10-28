//! Upstream `exec` postprocessor: run external commands on hook events.
//!
//! String commands run through the platform shell after replacing `{}`,
//! `{_path}`, `{_temppath}`, `{_directory}` and `{_filename}` (plus the
//! `_unc` variants) with quoted paths. List commands are executed directly
//! with each argument rendered as a keyword template.
use super::{template::Template, truthy};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Clone)]
enum Command {
    Shell(String),
    Args(Vec<Template>),
}

#[derive(Clone)]
pub struct ExecOptions {
    commands: Vec<Command>,
    asynchronous: bool,
    output: bool,
    verbose: bool,
    session: bool,
    /// Upstream `success`/`error`: actions run after each command.
    success: Option<crate::actions::Actions>,
    failure: Option<crate::actions::Actions>,
    /// Upstream `archive`: skip files this command already processed.
    archive: Option<std::sync::Arc<super::archive::ArchiveConfig>>,
    archive_session: std::sync::Arc<tokio::sync::OnceCell<Option<super::archive::ArchiveSession>>>,
}
impl std::fmt::Debug for ExecOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExecOptions { commands: [redacted] }")
    }
}

/// Path values an exec command may reference. Upstream's `_directory` keeps
/// its trailing separator.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecPaths<'a> {
    pub path: Option<&'a Path>,
    pub temppath: Option<&'a Path>,
    pub directory: Option<&'a Path>,
    pub filename: Option<&'a str>,
}
impl ExecPaths<'_> {
    fn directory(&self) -> String {
        match self.directory.or_else(|| self.path.and_then(Path::parent)) {
            Some(directory) if !directory.as_os_str().is_empty() => {
                let mut value = directory.to_string_lossy().into_owned();
                if !value.ends_with(std::path::MAIN_SEPARATOR) {
                    value.push(std::path::MAIN_SEPARATOR);
                }
                value
            }
            _ => String::new(),
        }
    }
    fn filename(&self) -> String {
        self.filename.map(str::to_owned).unwrap_or_else(|| {
            self.path
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
    }
    fn path(&self) -> String {
        self.path
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    fn temppath(&self) -> String {
        self.temppath
            .or(self.path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn command(value: &Value) -> Result<Command> {
    match value {
        Value::String(command) if !command.is_empty() => Ok(Command::Shell(command.clone())),
        Value::Array(args) if !args.is_empty() && args.len() <= 4096 => args
            .iter()
            .map(|arg| {
                arg.as_str()
                    .ok_or_else(|| error("exec command arguments must be strings"))
                    .and_then(Template::parse)
            })
            .collect::<Result<Vec<_>>>()
            .map(Command::Args),
        _ => Err(error("exec requires a command string or argument list")),
    }
}

/// POSIX `shlex.quote`; Windows wraps in double quotes like upstream.
pub fn quote(value: &str) -> String {
    if cfg!(windows) {
        return format!("\"{}\"", value.replace('"', "\\\""));
    }
    if value.is_empty() {
        return "''".into();
    }
    if value
        .chars()
        .all(|c| c.is_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return value.into();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Replace upstream's path placeholders (case-insensitive) in a shell command.
pub fn substitute(command: &str, paths: &ExecPaths<'_>) -> String {
    crate::sites::gdl::re!(r"(?i)\{(_directory(?:_unc)?|_filename|_(?:temp)?path(?:_unc)?|)\}")
        .replace_all(command, |captures: &regex::Captures<'_>| {
            quote(&match captures[1].to_ascii_lowercase().as_str() {
                "" | "_path" | "_path_unc" => paths.path(),
                "_temppath" | "_temppath_unc" => paths.temppath(),
                "_directory" | "_directory_unc" => paths.directory(),
                _ => paths.filename(),
            })
        })
        .into_owned()
}

impl ExecOptions {
    pub fn parse(options: &Map<String, Value>, base: &Path) -> Result<Self> {
        let commands = match options.get("commands").filter(|v| truthy(v)) {
            Some(Value::Array(commands)) if commands.len() <= 64 => {
                commands.iter().map(command).collect::<Result<Vec<_>>>()?
            }
            Some(_) => return Err(error("exec commands must be a list")),
            None => vec![command(
                options
                    .get("command")
                    .ok_or_else(|| error("exec requires a command"))?,
            )?],
        };
        let archive = super::archive::ArchiveConfig::parse(options, base)?
            .map(|config| std::sync::Arc::new(config.with_marker("_EXEC_")));
        let actions = |key: &str| {
            options
                .get(key)
                .filter(|v| !v.is_null())
                .map(crate::actions::Actions::parse)
                .transpose()
        };
        let flag = |key: &str, default: bool| options.get(key).map_or(default, truthy);
        Ok(Self {
            // Upstream only honors `async` for a single command.
            asynchronous: !options.get("commands").is_some_and(truthy) && flag("async", false),
            output: flag("output", true),
            verbose: flag("verbose", true),
            session: flag("session", false),
            archive,
            archive_session: Default::default(),
            success: actions("success")?,
            failure: actions("error")?,
            commands,
        })
    }

    /// Run each command in order, stopping after the first non-zero exit.
    /// Like upstream, a failing command logs a warning instead of failing the file.
    /// The archive session, opened for the first file like upstream.
    async fn archive_session(
        &self,
        context: &Map<String, Value>,
        media: &crate::extract::Media,
        root: &Path,
        directory: &Path,
    ) -> Option<&super::archive::ArchiveSession> {
        let config = self.archive.as_ref()?;
        self.archive_session
            .get_or_init(|| async {
                let descriptor = crate::source_archive::for_media(media);
                let category = descriptor
                    .as_ref()
                    .map(|d| d.category.clone())
                    .unwrap_or_else(|| media.category.clone());
                match config
                    .open(
                        &category,
                        descriptor.as_ref().map(|d| d.format.as_str()),
                        context,
                        root,
                        directory,
                    )
                    .await
                {
                    Ok(session) => Some(session),
                    Err(failure) => {
                        eprintln!("warning: failed to open exec archive: {failure}");
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    /// [`ExecOptions::run`] with archive checks for `media`.
    pub async fn run_archived(
        &self,
        context: &mut Map<String, Value>,
        paths: ExecPaths<'_>,
        media: &crate::extract::Media,
        root: &Path,
    ) -> Result<()> {
        let directory = paths
            .directory
            .map(Path::to_path_buf)
            .or_else(|| paths.path.and_then(Path::parent).map(Path::to_path_buf))
            .unwrap_or_else(|| root.to_path_buf());
        let Some(session) = self.archive_session(context, media, root, &directory).await else {
            return self.run(context, paths).await;
        };
        let check = session.check(context).await?;
        if check.hit {
            return Ok(());
        }
        self.run(context, paths).await?;
        session.add(context).await
    }

    pub async fn run(&self, context: &mut Map<String, Value>, paths: ExecPaths<'_>) -> Result<()> {
        if self.commands.iter().any(|c| matches!(c, Command::Args(_))) {
            context.insert("_directory".into(), paths.directory().into());
            context.insert("_filename".into(), paths.filename().into());
            context.insert("_temppath".into(), paths.temppath().into());
            context.insert("_path".into(), paths.path().into());
        }
        for command in &self.commands {
            let (mut process, display) = match command {
                Command::Shell(command) => {
                    let command = substitute(command, &paths);
                    let process = if cfg!(windows) {
                        let mut process = tokio::process::Command::new("cmd");
                        process.arg("/C").arg(&command);
                        process
                    } else {
                        let mut process = tokio::process::Command::new("sh");
                        process.arg("-c").arg(&command);
                        process
                    };
                    let display = if self.verbose {
                        command.clone()
                    } else {
                        command.split(' ').next().unwrap_or_default().to_owned()
                    };
                    (process, display)
                }
                Command::Args(templates) => {
                    let mut args = templates
                        .iter()
                        .map(|template| template.render(context))
                        .collect::<Result<Vec<_>>>()?;
                    args[0] = super::expand_home(&args[0]).to_string_lossy().into_owned();
                    let mut process = tokio::process::Command::new(&args[0]);
                    process.args(&args[1..]);
                    let display = if self.verbose {
                        format!("{args:?}")
                    } else {
                        args[0].clone()
                    };
                    (process, display)
                }
            };
            if !self.output {
                process
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
            }
            #[cfg(unix)]
            if self.session {
                process.process_group(0);
            }
            // Upstream: `CREATE_NEW_PROCESS_GROUP` on Windows.
            #[cfg(windows)]
            if self.session {
                process.creation_flags(0x0000_0200);
            }
            let mut child = process
                .spawn()
                .map_err(|e| error(format!("exec failed to start '{display}': {e}")))?;
            if self.asynchronous {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                return Ok(());
            }
            let status = child.wait().await?;
            if !status.success() {
                crate::actions::log_as(
                    "postprocessor.exec",
                    crate::actions::WARNING,
                    &format!(
                        "'{display}' returned with non-zero exit status ({})",
                        status.code().unwrap_or(-1)
                    ),
                )?;
                if let Some(actions) = &self.failure {
                    actions.run(Some(context)).await?;
                }
                break;
            }
            if let Some(actions) = &self.success {
                actions.run(Some(context)).await?;
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn options(value: Value) -> ExecOptions {
        ExecOptions::parse(value.as_object().unwrap(), Path::new("/")).unwrap()
    }

    #[test]
    fn substitutes_quoted_paths_like_shlex() {
        let path = Path::new("/tmp/a b/it's.jpg");
        let paths = ExecPaths {
            path: Some(path),
            temppath: Some(Path::new("/tmp/a b/it's.jpg.part")),
            ..Default::default()
        };
        assert_eq!(
            substitute("x {} {_PATH} {_directory} {_filename} {_temppath}", &paths),
            "x '/tmp/a b/it'\"'\"'s.jpg' '/tmp/a b/it'\"'\"'s.jpg' '/tmp/a b/' \
             'it'\"'\"'s.jpg' '/tmp/a b/it'\"'\"'s.jpg.part'"
        );
        assert_eq!(quote("/plain/path.jpg"), "/plain/path.jpg");
        assert_eq!(quote(""), "''");
    }

    #[tokio::test]
    async fn runs_shell_and_list_commands_in_order_and_stops_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("media file.jpg");
        std::fs::write(&file, b"x").unwrap();
        let log = dir.path().join("log");
        let exec = options(json!({"commands": [
            format!("echo shell {{_filename}} >> '{}'", log.display()),
            ["sh", "-c", format!("echo list $0 >> '{}'", log.display()), "{id}-{_filename}"],
            "false",
            format!("echo unreachable >> '{}'", log.display()),
        ]}));
        let mut context = Map::from_iter([("id".into(), json!(7))]);
        exec.run(
            &mut context,
            ExecPaths {
                path: Some(&file),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "shell media file.jpg\nlist 7-media file.jpg\n"
        );
        assert_eq!(context["_filename"], "media file.jpg");
    }

    #[tokio::test]
    async fn error_actions_stop_extraction() {
        let exec = options(json!({"command": "false", "error": "terminate"}));
        let failure = exec.run(&mut Map::new(), ExecPaths::default()).await;
        assert_eq!(
            crate::Stop::of(&failure.unwrap_err()),
            Some(crate::Stop::Terminate)
        );
        let exec = options(json!({"command": "true", "success": "keyword done 1"}));
        let mut context = Map::new();
        exec.run(&mut context, ExecPaths::default()).await.unwrap();
        assert_eq!(context["done"], 1);
    }

    #[test]
    fn rejects_unsupported_options_and_missing_commands() {
        let parse = |v: Value| ExecOptions::parse(v.as_object().unwrap(), Path::new("/"));
        assert!(parse(json!({})).is_err());
        assert!(parse(json!({"command": []})).is_err());
        assert!(parse(json!({"command": "x", "archive": "a.db"})).is_ok());
        assert!(parse(json!({"command": "x", "async": true})).is_ok());
        assert!(parse(json!({"command": "x", "error": "abort", "success": ["print ok"]})).is_ok());
        assert!(parse(json!({"command": "x", "error": "nonsense"})).is_err());
    }
}
