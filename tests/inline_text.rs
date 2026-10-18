use gallery_dl_rs::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions, Outcome},
    extract::{self, Media, MediaJob},
    size::{SizeLimits, SizeRejection},
};
use serde_json::json;
use std::time::Duration;

fn item(data: &str) -> Media {
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, format!("text:{data}"), {
            "filename": "article", "extension": "txt", "tweet_id": "123",
            "num": 1
        }]]))
        .unwrap(),
        "twitter",
    )
    .unwrap()
    .remove(0)
}

#[test]
fn literal_text_preserves_unicode_newlines_and_percent_sequences() {
    let data = "\nÆøå 🍂\r\n100% %20 ?q=#fragment\tend\n";
    let media = item(data);
    assert!(matches!(&media.job, MediaJob::InlineText { data: actual } if actual == data));
    assert!(media.url.as_str().contains("%2520"));
    assert!(media.url.as_str().contains("%0D%0A"));
    assert!(media.url.query().is_none());
    assert!(media.url.fragment().is_none());
    let public = serde_json::to_value(&media).unwrap();
    assert_eq!(public["job"], json!({"kind": "inline_text"}));
    assert!(media.handoff.is_empty());
    let mut other = media.clone();
    other.job = MediaJob::InlineText {
        data: "different content".into(),
    };
    assert_ne!(media.identity(), other.identity());
    // Even when caller-provided opaque URLs normalize away line breaks, exact
    // bytes in the job distinguish destinations and request identities.
    let mut normalized = item("a\nb");
    let mut joined = item("ab");
    normalized.url = url::Url::parse("text:a\nb").unwrap();
    joined.url = url::Url::parse("text:ab").unwrap();
    assert_eq!(normalized.url, joined.url);
    assert_ne!(normalized.identity(), joined.identity());
}

#[test]
fn malformed_transport_and_oversized_text_fail_without_payloads() {
    for source in [
        "file:/private/secret",
        "data:text/plain,secret",
        "ftp://example.com/secret",
        "ytdl:text:secret",
    ] {
        let error = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, source, {}]])).unwrap(),
            "twitter",
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("secret"));
    }
    for metadata in [
        json!({"_ytdl_manifest": "hls"}),
        json!({"_ytdl_manifest_data": "secret"}),
    ] {
        let error = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "text:secret", metadata]])).unwrap(),
            "twitter",
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("secret"));
    }
    assert!(extract::inline_text("x".repeat(16 * 1024 * 1024)).is_ok());
    let error = extract::inline_text("secret".repeat(3 * 1024 * 1024))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret"));
}

#[tokio::test]
async fn inline_text_is_atomic_exact_utf8_and_uses_sidecars_and_archive() {
    let root = tempfile::tempdir().unwrap();
    let data = "<p>føø &amp; 🍂\r\n100% %20</p>\n";
    let media = item(data);
    let client = download::client(Duration::from_secs(1))
        .unwrap()
        .with_resume(true);
    let options = DownloadOptions {
        retries: 7,
        write_metadata: true,
        limits: SizeLimits::new(Some(data.len() as u64), Some(data.len() as u64)).unwrap(),
        ..Default::default()
    };
    let archive = Archive::open(root.path().join("archive.sqlite3"))
        .await
        .unwrap();
    let outcome = archive
        .download(&client, root.path(), &media, options)
        .await
        .unwrap();
    let ArchiveOutcome::Completed(Outcome::Downloaded(path)) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(std::fs::read(&path).unwrap(), data.as_bytes());
    let sidecar = std::fs::read_to_string(format!("{}.json", path.display())).unwrap();
    assert!(!sidecar.contains("text:"));
    assert!(!sidecar.contains("100%"));
    let entries: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|v| v.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 2, "no HTTP resume or temporary artifacts");
    assert!(matches!(
        download::download_with_options(&client, root.path(), &media, options)
            .await
            .unwrap(),
        Outcome::Skipped(_)
    ));
    // Direct archive identity is available for all jobs and incorporates data.
    let mut direct = media.clone();
    direct.category = "direct".into();
    assert!(matches!(
        archive
            .download(&client, root.path(), &direct, options)
            .await
            .unwrap(),
        ArchiveOutcome::Completed(Outcome::Downloaded(_))
    ));
    assert!(matches!(
        archive
            .download(&client, root.path(), &direct, options)
            .await
            .unwrap(),
        ArchiveOutcome::AlreadyArchived
    ));
}

#[tokio::test]
async fn inline_size_filtering_leaves_no_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let media = item("Æ"); // two UTF-8 bytes
    let client = download::client(Duration::from_secs(1))
        .unwrap()
        .with_resume(true);
    for limits in [
        SizeLimits::new(Some(3), None).unwrap(),
        SizeLimits::new(None, Some(1)).unwrap(),
    ] {
        let result = download::download_with_options(
            &client,
            root.path(),
            &media,
            DownloadOptions {
                limits,
                write_metadata: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Outcome::Filtered(
                SizeRejection::TooSmall { size: 2, .. } | SizeRejection::TooLarge { size: 2, .. }
            )
        ));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn sidecar_keeps_article_description_without_html_urls_or_private_fields() {
    let root = tempfile::tempdir().unwrap();
    let media = extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "text:article body", {
            "filename": "article", "extension": "html", "tweet_id": "123", "num": 1,
            "article": {"id": "456", "title": "A title", "date": 1700000000,
                "date_updated": 1700000001,
                "html": "<img src=\"https://example.com/image?secret-signature\">",
                "private_token": "private-value", "url": "https://private.example/"}
        }]]))
        .unwrap(),
        "twitter",
    )
    .unwrap()
    .remove(0);
    assert!(
        media
            .metadata
            .as_ref()
            .unwrap()
            .article
            .as_ref()
            .unwrap()
            .html
            .is_some()
    );
    let client = download::client(Duration::from_secs(1)).unwrap();
    let result = download::download_with_options(
        &client,
        root.path(),
        &media,
        DownloadOptions {
            write_metadata: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let path = result.saved_path().unwrap();
    let sidecar = std::fs::read_to_string(format!("{}.json", path.display())).unwrap();
    let document: serde_json::Value = serde_json::from_str(&sidecar).unwrap();
    assert_eq!(
        document["metadata"]["article"],
        json!({
            "id": "456", "title": "A title", "date": 1700000000, "date_updated": 1700000001
        })
    );
    for secret in [
        "\"html\"",
        "secret-signature",
        "private-value",
        "private.example",
        "text:",
    ] {
        assert!(
            !sidecar.contains(secret),
            "unexpected sidecar field {secret}"
        );
    }
    assert!(
        media
            .metadata
            .as_ref()
            .unwrap()
            .article
            .as_ref()
            .unwrap()
            .html
            .is_some(),
        "sanitizing a sidecar must not change inspection metadata"
    );
}
