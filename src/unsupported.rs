//! Upstream `--write-unsupported FILE`: record queued URLs that no extractor
//! handles, one per line.
use std::{
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
};

static SINK: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// Truncate and open `path` as the destination for unsupported URLs.
pub fn set_file(path: &Path) -> crate::Result<()> {
    let file = std::fs::File::create(path)?;
    SINK.set(Mutex::new(file))
        .map_err(|_| crate::error("unsupported URL file is already configured"))
}

/// Record a URL no extractor supports.
pub fn record(url: &str) {
    if let Some(sink) = SINK.get()
        && let Ok(mut file) = sink.lock()
    {
        let _ = writeln!(file, "{url}");
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    #[tokio::test]
    async fn skipped_external_children_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsupported.txt");
        super::set_file(&path).unwrap();
        let child = url::Url::parse("https://unsupported.invalid/x").unwrap();
        let events = crate::sites::fail(Box::new(crate::sites::ExternalChild(child)));
        let queue: crate::event::Queue<'_> = std::sync::Arc::new(|_| None);
        let events: Vec<_> = crate::sites::with_external_children(events, queue)
            .collect()
            .await;
        assert!(events.is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "https://unsupported.invalid/x\n"
        );
    }
}
