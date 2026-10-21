//! Actual Source Prepare hash callbacks reading cached PathFormat paths.
use super::source_formatter_date_tests::source_templates;
use crate::{
    archive::{Archive, ArchiveOutcome},
    booru::{
        Booru, Site,
        defaults_tests::{replies, serve},
    },
    download::{self, DownloadOptions},
    event::ExtractionEvent,
    network::NetworkOptions,
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

const EVENTS: &[&str] = &["prepare", "file", "skip", "after"];
const KEYS: &[&str] = &[
    "id",
    "filename",
    "extension",
    "previous",
    "first",
    "second",
    "third",
];
fn identity(id: &str) -> String {
    format!(
        "danbooru:v1:{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(json!([id]).to_string().as_bytes())
    )
}
fn entries(path: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .prepare("SELECT identity FROM completed_media ORDER BY identity")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn artifacts(root: &Path) -> Value {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<Value>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
                continue;
            }
            let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
            if relative.starts_with("oracle-") || relative.starts_with("history.sqlite3") {
                continue;
            }
            files.push(json!({"path":relative,"data_hex":hex(&std::fs::read(&path).unwrap())}));
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    json!(files)
}

#[tokio::test]
async fn actual_source_prepare_hash_reads_job_cached_paths() {
    let oracle: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_prepare_cache.json"
    ))
    .unwrap();
    let cases = oracle["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 12);
    let mut replayed = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        if case.get("custom_emitter").is_some() {
            // The synthetic emitter mutates one shared Directory dictionary
            // per Url; native Post scopes carry immutable post keywords, so
            // this grouping has no equivalent native event sequence.
            assert_eq!(name, "custom_grouped_same_post");
            continue;
        }
        replayed += 1;
        let root = tempfile::tempdir().unwrap();
        let (filename, directory) = source_templates(case, root.path());
        for seed in case["preseed"].as_array().unwrap() {
            let path = root.path().join(seed["path"].as_str().unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let data = seed["data_hex"].as_str().unwrap();
            let bytes = (0..data.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&data[i..i + 2], 16).unwrap())
                .collect::<Vec<_>>();
            std::fs::write(path, bytes).unwrap();
        }
        let archive_path = root.path().join("history.sqlite3");
        let archive = Archive::open(&archive_path).await.unwrap();
        let archived = !case["archive_entries_before"]
            .as_array()
            .unwrap()
            .is_empty();
        for entry in case["archive_entries_before"].as_array().unwrap() {
            let id = entry.as_str().unwrap().rsplit('_').next().unwrap();
            archive.record(&identity(id)).await.unwrap();
        }
        let failed = case["failed_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_u64().unwrap())
            .collect::<Vec<_>>();
        for job in case["jobs"].as_array().unwrap() {
            let ordinal = job["ordinal"].as_u64().unwrap();
            let name = format!("{name}#{ordinal}");
            for event in EVENTS {
                let _ = std::fs::remove_file(root.path().join(format!("oracle-{event}.jsonl")));
            }
            let source = case["url"].as_str().unwrap().parse().unwrap();
            let (api_root, server) = serve(replies(job, Site::Danbooru, &source));
            let mut configs = job["hook_options"].as_array().unwrap().clone();
            for event in EVENTS {
                configs.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root.path(),"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":KEYS}));
            }
            let processors = Arc::new(Postprocessors::new(
                configs
                    .iter()
                    .map(|value| Specification::parse(value, root.path()).unwrap())
                    .collect(),
            ));
            let client = Booru::new(Site::Danbooru, Duration::from_secs(5))
                .unwrap()
                .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
                .unwrap()
                .with_api_root(api_root);
            let bodies = case["download_body_hex_by_id"].clone();
            let failed = failed.clone();
            let input = client
                .events(&source)
                .map(move |result| {
                    result.map(|mut event| {
                        if let ExtractionEvent::Media(media) = &mut event {
                            let id = media.metadata.as_ref().unwrap().post_id.clone();
                            if failed.iter().any(|failed| failed.to_string() == id) {
                                media.url = "http://127.0.0.1:1/failed.jpg".parse().unwrap();
                                media.job = crate::extract::MediaJob::DirectHttp;
                            } else {
                                let data = bodies[&id].as_str().unwrap();
                                let data = (0..data.len())
                                    .step_by(2)
                                    .map(|i| {
                                        u8::from_str_radix(&data[i..i + 2], 16).unwrap() as char
                                    })
                                    .collect();
                                media.job = crate::extract::MediaJob::InlineText { data };
                            }
                        }
                        event
                    })
                })
                .boxed();
            let configuration = JobConfiguration {
                processors: processors.clone(),
                root: root.path().to_owned(),
                filename: filename.clone(),
                directory: directory.clone(),
                init: false,
            };
            let mut output = jobs::apply_with_descriptor(
                input,
                Arc::new(BTreeMap::from([("danbooru".into(), configuration)])),
                Some("danbooru".into()),
                crate::source_archive::for_url(&source),
            );
            let download = download::client(Duration::from_secs(2)).unwrap();
            let mut errors = Vec::new();
            while let Some(result) = output.next().await {
                let item = match result {
                    Ok(item) => item,
                    Err(error) => {
                        errors.push(error.to_string());
                        continue;
                    }
                };
                let options = DownloadOptions {
                    postprocessors: Some(&processors),
                    postprocessor_scope: item.scope.as_ref(),
                    directory: directory.as_ref(),
                    ..Default::default()
                };
                match archive
                    .download_with_template(
                        &download,
                        root.path(),
                        &item.media,
                        options,
                        filename.as_ref(),
                    )
                    .await
                {
                    Ok(ArchiveOutcome::Completed(_) | ArchiveOutcome::AlreadyArchived) => {}
                    Err(error) => errors.push(error.to_string()),
                }
            }
            assert_eq!(
                server.join().unwrap().len(),
                job["calls"].as_array().unwrap().len(),
                "{name}: API requests"
            );
            assert_eq!(
                errors.is_empty(),
                job["status"] == 0,
                "{name}: job status {} errors {errors:?}",
                job["status"]
            );
            let mut published = job["published"].as_array().unwrap().clone();
            if case["name"] == "three_prepare_hashes_keep_temporary_path_after_second_rebuild" {
                // Source downloads into the first-built temppath, overwriting
                // the preseeded file, then renames it to the rebuilt realpath.
                // Native writes the final path and leaves the unrelated file.
                published.extend(case["preseed"].as_array().unwrap().iter().cloned());
                published.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            }
            assert_eq!(
                artifacts(root.path()),
                json!(published),
                "{name}: published files"
            );
            if archived {
                let mut expected = job["archive_entries_after"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| identity(entry.as_str().unwrap().rsplit('_').next().unwrap()))
                    .collect::<Vec<_>>();
                expected.sort();
                assert_eq!(entries(&archive_path), expected, "{name}: archive entries");
            }
            for event in EVENTS {
                let path = root.path().join(format!("oracle-{event}.jsonl"));
                let actual = std::fs::read_to_string(path)
                    .unwrap_or_default()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>();
                let expected = job["trace"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|trace| trace["event"] == *event)
                    .collect::<Vec<_>>();
                assert_eq!(actual.len(), expected.len(), "{name}: {event} callbacks");
                for (actual, expected) in actual.iter().zip(expected) {
                    for key in KEYS {
                        assert_eq!(
                            actual.get(*key),
                            expected["metadata"].get(*key),
                            "{name}: {event} {key} {actual}"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(replayed, 11);
}
