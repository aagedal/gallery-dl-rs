use gallery_dl_rs::{
    download::{self, DownloadOptions, Outcome},
    extract::{self, Media},
    naming::{DirectoryTemplate, FilenameTemplate},
};
use serde_json::{Value, json};
use std::time::Duration;

fn item(extra: Value) -> Media {
    let mut fields = json!({"tweet_id":"123","num":1,"filename":"photo","extension":"jpg"});
    fields
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    extract::parse_messages(
        &serde_json::to_vec(&json!([[
            3,
            "https://cdn.example/photo.jpg?token=SIGNED_SECRET",
            fields
        ]]))
        .unwrap(),
        "twitter",
    )
    .unwrap()
    .remove(0)
}

#[test]
fn compatibility_file_metadata_is_typed_allowlisted_and_identity_independent() {
    let original = item(json!({}));
    let media = item(
        json!({"width":1920,"height":1080,"width_original":4096,"height_original":2160,
        "duration":1.25,"bitrate":2000000,"type":"video","description":"Alt text",
        "sensitive_flags":["Nudity","Violence","PRIVATE_UNKNOWN"],"source_id":"999",
        "source_user":{"name":"author","private":"PRIVATE_UNKNOWN"},"private_url":"PRIVATE_UNKNOWN"}),
    );
    let details = media.metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!((details.width, details.height), (Some(1920), Some(1080)));
    assert_eq!(
        (details.width_original, details.height_original),
        (Some(4096), Some(2160))
    );
    assert_eq!(details.duration, Some(1.25));
    assert_eq!(details.bitrate, Some(2000000));
    assert_eq!(details.media_type.as_deref(), Some("video"));
    assert_eq!(details.description.as_deref(), Some("Alt text"));
    assert_eq!(details.source_id.as_deref(), Some("999"));
    assert_eq!(details.source_username.as_deref(), Some("author"));
    assert_eq!(
        details
            .sensitive_flags
            .iter()
            .map(|value| value.as_ref())
            .collect::<Vec<_>>(),
        ["Nudity", "Violence"]
    );
    assert_eq!(media.identity(), original.identity());
    assert_eq!(media.archive_key(), original.archive_key());
    assert_eq!(media.filename, original.filename);
    assert!(
        !serde_json::to_string(&media)
            .unwrap()
            .contains("PRIVATE_UNKNOWN")
    );
}

#[test]
fn invalid_optional_fields_are_omitted_without_changing_valid_jobs() {
    let media = item(
        json!({"width":-1,"height":0,"width_original":"1024","height_original":10.5,
        "duration":-0.1,"bitrate":-1,"type":"PRIVATE_UNKNOWN","description":false,
        "sensitive_flags":[{},"PRIVATE_UNKNOWN"],"source_id":"0999","source_user":false,
        "file":{"width":9999,"url":"PRIVATE_UNKNOWN"}}),
    );
    assert!(media.metadata.as_ref().unwrap().file.is_none());
    for duration in [json!("1.25"), json!(null), json!({})] {
        assert!(
            item(json!({"duration":duration}))
                .metadata
                .unwrap()
                .file
                .is_none()
        );
    }
    let aliases = item(
        json!({"image_width":640,"image_height":480,"duration":0,"bitrate":0,"type":"GraphImage","source_id":999}),
    );
    let file = aliases.metadata.unwrap().file.unwrap();
    assert_eq!(file.width, Some(640));
    assert_eq!(file.height, Some(480));
    assert_eq!(file.duration, Some(0.0));
    assert_eq!(file.bitrate, Some(0));
    assert_eq!(file.media_type.as_deref(), Some("image"));
    assert_eq!(file.source_id.as_deref(), Some("999"));
}

#[test]
fn file_fields_render_in_filename_and_directory_templates_without_inference() {
    let media =
        item(json!({"width":1920,"height":1080,"duration":1.25,"bitrate":2000000,"type":"video"}));
    let template =
        FilenameTemplate::parse("{width}x{height}_{duration}_{bitrate}_{media_type}.{extension}")
            .unwrap();
    assert_eq!(
        template.render(&media).unwrap(),
        "1920x1080_1.25_2000000_video.jpg"
    );
    let directories =
        DirectoryTemplate::parse(&["{media_type}".into(), "{width}x{height}".into()]).unwrap();
    assert_eq!(
        directories.render(&media).unwrap(),
        std::path::Path::new("video").join("1920x1080")
    );
    assert!(template.render(&item(json!({}))).is_err());
    assert!(
        FilenameTemplate::parse("{width_original}")
            .unwrap()
            .render(&media)
            .is_err()
    );
    let mut invalid = media;
    invalid
        .metadata
        .as_mut()
        .unwrap()
        .file
        .as_mut()
        .unwrap()
        .duration = Some(f64::INFINITY);
    assert!(
        FilenameTemplate::parse("{duration}")
            .unwrap()
            .render(&invalid)
            .is_err()
    );
}

#[tokio::test]
async fn sidecars_preserve_file_metadata_without_private_transport_fields() {
    let media = item(
        json!({"width":1920,"height":1080,"description":"Alt text", "source_id":"999",
        "_http_headers":{"Authorization":"PRIVATE_SECRET"},"_fallback":["https://cdn.example/PRIVATE_SECRET"]}),
    );
    let root = tempfile::tempdir().unwrap();
    let path = download::destination(root.path(), &media);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"existing").unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    assert_eq!(
        download::download_with_options(
            &client,
            root.path(),
            &media,
            DownloadOptions {
                write_metadata: true,
                ..Default::default()
            }
        )
        .await
        .unwrap(),
        Outcome::Skipped(path.clone())
    );
    let text = std::fs::read_to_string(format!("{}.json", path.display())).unwrap();
    assert!(!text.contains("SECRET"));
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["metadata"]["file"],
        json!({"width":1920,"height":1080,"description":"Alt text","source_id":"999"})
    );
}
