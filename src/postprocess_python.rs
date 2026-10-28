//! Upstream `python` postprocessor, run in an external Python interpreter.
//!
//! The keyword dictionary is sent to `python3` (or `$GALLERY_DL_PYTHON`) as
//! JSON; the interpreter calls the configured `function` (`MODULE:NAME`,
//! where MODULE is an importable name or a file path) or evaluates the
//! `expression` (`mode: "eval"`) with the keywords as variables, and the
//! possibly modified dictionary is read back. Values that are not JSON
//! serializable come back as strings.
use super::truthy;
use crate::{Result, error};
use serde_json::{Map, Value};
use std::path::Path;

const SCRIPT: &str = r#"
import importlib, importlib.util, json, os, sys
spec = json.loads(sys.argv[1])
kwdict = json.load(sys.stdin)
if spec.get("expression") is not None:
    eval(spec["expression"], {"__builtins__": __builtins__}, kwdict)
else:
    module_name, _, function_name = spec["function"].rpartition(":")
    path = os.path.expanduser(module_name)
    if os.sep in path or path.endswith(".py"):
        loader = importlib.util.spec_from_file_location("gdl_user_module", path)
        module = importlib.util.module_from_spec(loader)
        loader.loader.exec_module(module)
    else:
        module = importlib.import_module(module_name)
    getattr(module, function_name)(kwdict)
json.dump(kwdict, sys.stdout, default=str)
"#;

#[derive(Clone, Debug)]
pub struct PythonOptions {
    function: Option<String>,
    expression: Option<String>,
    archive: Option<std::sync::Arc<super::archive::ArchiveConfig>>,
    session: std::sync::Arc<tokio::sync::OnceCell<Option<super::archive::ArchiveSession>>>,
}

impl PythonOptions {
    pub fn parse(options: &Map<String, Value>, base: &Path) -> Result<Self> {
        let string = |key: &str| options.get(key).and_then(Value::as_str).map(str::to_owned);
        let eval = string("mode").as_deref() == Some("eval")
            || string("mode").is_none() && options.get("expression").is_some_and(truthy);
        let (function, expression) = if eval {
            let expression = string("expression")
                .ok_or_else(|| error("python eval mode needs an expression"))?;
            (None, Some(expression))
        } else {
            let function = string("function").ok_or_else(|| error("python needs a function"))?;
            if !function.contains(':') {
                return Err(error("python function must be MODULE:NAME"));
            }
            (Some(function), None)
        };
        let archive = super::archive::ArchiveConfig::parse(options, base)?
            .map(|config| std::sync::Arc::new(config.with_marker("_PYTHON_")));
        Ok(Self {
            function,
            expression,
            archive,
            session: Default::default(),
        })
    }

    fn interpreter() -> String {
        std::env::var("GALLERY_DL_PYTHON").unwrap_or_else(|_| "python3".into())
    }

    /// Call the function with `context`, replacing it with the result.
    pub async fn run(&self, context: &mut Map<String, Value>) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let spec = serde_json::json!({
            "function": self.function,
            "expression": self.expression,
        });
        let mut child = tokio::process::Command::new(Self::interpreter())
            .arg("-c")
            .arg(SCRIPT)
            .arg(spec.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| error(format!("python postprocessor could not start Python: {e}")))?;
        let input = serde_json::to_vec(context)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| error("python stdin unavailable"))?;
        stdin.write_all(&input).await?;
        drop(stdin);
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            return Err(error("python postprocessor function failed"));
        }
        if let Ok(Value::Object(result)) = serde_json::from_slice::<Value>(&output.stdout) {
            *context = result;
        }
        Ok(())
    }

    /// [`PythonOptions::run`] with the optional `archive`.
    pub async fn run_archived(
        &self,
        context: &mut Map<String, Value>,
        media: &crate::extract::Media,
        root: &Path,
        directory: &Path,
    ) -> Result<()> {
        let Some(config) = &self.archive else {
            return self.run(context).await;
        };
        let session = self
            .session
            .get_or_init(|| async {
                let descriptor = crate::source_archive::for_media(media);
                let category = descriptor
                    .as_ref()
                    .map(|d| d.category.clone())
                    .unwrap_or_else(|| media.category.clone());
                config
                    .open(
                        &category,
                        descriptor.as_ref().map(|d| d.format.as_str()),
                        context,
                        root,
                        directory,
                    )
                    .await
                    .map_err(|e| eprintln!("warning: failed to open python archive: {e}"))
                    .ok()
            })
            .await;
        let Some(session) = session else {
            return self.run(context).await;
        };
        if session.check(context).await?.hit {
            return Ok(());
        }
        self.run(context).await?;
        session.add(context).await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn calls_functions_and_expressions_with_the_keywords() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("mod.py");
        std::fs::write(
            &module,
            "def f(kw):\n    kw['title'] = kw['title'].upper()\n",
        )
        .unwrap();
        let options = PythonOptions::parse(
            json!({"function": format!("{}:f", module.display())})
                .as_object()
                .unwrap(),
            dir.path(),
        )
        .unwrap();
        let mut context = json!({"title": "abc", "id": 1})
            .as_object()
            .unwrap()
            .clone();
        options.run(&mut context).await.unwrap();
        assert_eq!(context["title"], "ABC");

        let eval = PythonOptions::parse(
            json!({"mode": "eval", "expression": "kwdict_unused if False else None"})
                .as_object()
                .unwrap(),
            dir.path(),
        )
        .unwrap();
        eval.run(&mut context).await.unwrap();
        assert_eq!(context["id"], 1);
        assert!(
            PythonOptions::parse(json!({"function": "x"}).as_object().unwrap(), dir.path())
                .is_err()
        );
    }
}
