//! Replay real DownloadJob/MetadataPP archive fixtures through actual native files.
//! Source writes directly to its sidecar and leaves an empty file on encoding
//! failure. Native publication is atomic: a failed write preserves an old file
//! (or creates none). That intentional artifact difference is asserted below.
use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    download::{self, DownloadOptions},
    event::{ChildMetadata, ChildScope, ExtractionEvent, KeywordPolicy, KeywordUpdate, Post},
    extract::{Media, MediaJob, PostMetadata},
    naming::{DirectoryTemplate, FilenameTemplate},
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
    source_archive::ArchiveDescriptor,
};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};

const KEYS: &[&str] = &[
    "id",
    "num",
    "phase",
    "payload",
    "scope",
    "removed",
    "changed",
    "metadata_path",
    "_archive_metadata",
    "category",
    "subcategory",
    "filename",
    "extension",
    "init_seen",
    "child_seen",
];
fn descriptor() -> ArchiveDescriptor {
    ArchiveDescriptor {
        category: "fixture".into(),
        format: "{id}_{num}".into(),
    }
}
fn normalize(value: &mut Value, root: &Path) {
    match value {
        Value::String(s) => {
            *s = s.replace(root.to_str().unwrap(), "$WORK");
            // Oracles were recorded on POSIX; joined paths are native here.
            if cfg!(windows) && s.starts_with("$WORK") {
                *s = s.replace('\\', "/");
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| normalize(v, root)),
        Value::Object(items) => items.values_mut().for_each(|v| normalize(v, root)),
        _ => (),
    }
}
fn source_events(input: &Value, phase: &str) -> Vec<gallery_dl_rs::Result<ExtractionEvent>> {
    let fields = json!({"id":1,"num":0,"phase":phase,"payload":"first", "scope":"directory","removed":"present","extension":"txt", "category":"fixture","subcategory":"archive-job"});
    let mut events = vec![Ok(ExtractionEvent::Post(Box::new(Post {
        keywords: KeywordPolicy::Shared {
            feedback: None,
            objects: Arc::from([]),
            post_fields: Arc::new(fields.as_object().unwrap().clone()),
            file_fields: ["num", "payload", "index", "filename", "name", "extension"]
                .map(String::from)
                .into(),
        },
        category: "fixture".into(),
        metadata: Some(PostMetadata {
            post_id: "1".into(),
            ..Default::default()
        }),
        count: None,
    })))];
    for index in 1..=2 {
        let payload = input
            .get("payloads")
            .and_then(|v| v.get(index - 1))
            .cloned()
            .unwrap_or_else(|| json!(if index == 1 { "first" } else { "second" }));
        events.push(Ok(ExtractionEvent::Media(Box::new(Media {
            url: format!("https://fixture.invalid/{index}.txt")
                .parse()
                .unwrap(),
            job: MediaJob::InlineText {
                data: format!("body-{index}"),
            },
            category: "fixture".into(),
            filename: format!("file-{phase}-1-{index}.txt"),
            metadata: Some(PostMetadata {
                post_id: "1".into(),
                media_id: format!("1_{index}"),
                index,
                source_num: Some(json!(index)),
                keyword_updates: Some(
                    vec![
                        // The synthetic Source emits no filename keyword.
                        KeywordUpdate::Remove("filename".into()),
                        KeywordUpdate::Set {
                            name: "num".into(),
                            value: json!(index),
                        },
                        KeywordUpdate::Set {
                            name: "payload".into(),
                            value: payload,
                        },
                    ]
                    .into(),
                ),
                ..Default::default()
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        }))))
    }
    if input["empty_child"] == true {
        // A live shared queue preserves the source dictionary across child hooks.
        let child = ChildScope::new(
            ChildMetadata::from_raw("fixture", "empty", &Map::new())
                .with_source_archive(descriptor())
                .with_keyword_updates(vec![KeywordUpdate::Set {
                    name: "num".into(),
                    value: json!(2),
                }]),
        );
        events.push(Ok(ExtractionEvent::BeginChild(child.clone())));
        events.push(Ok(ExtractionEvent::EndChild(child.control)));
    }
    events.push(Ok(ExtractionEvent::EndPost));
    events
}
fn tables(path: &Path) -> Value {
    let db = Connection::open(path).unwrap();
    let mut query = db
        .prepare("SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let tables = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    Value::Array(
        tables
            .map(|table| {
                let (name, sql) = table.unwrap();
                // SQLite stores harmless identifier quoting verbatim in sqlite_master.
                let sql = sql.replace(
                    &format!("CREATE TABLE \"{}\"", name.replace('"', "\"\"")),
                    &format!("CREATE TABLE {name}"),
                );
                let mut entries = db
                    .prepare(&format!(
                        "SELECT entry FROM \"{}\" ORDER BY entry",
                        name.replace('"', "\"\"")
                    ))
                    .unwrap();
                let rows = entries
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .map(|v| json!([v.unwrap()]))
                    .collect::<Vec<_>>();
                json!({"name":name,"sql":sql,"rows":rows})
            })
            .collect(),
    )
}

fn normalized_tables(mut value: Value) -> Value {
    for table in value.as_array_mut().unwrap() {
        let name = table["name"].as_str().unwrap();
        let sql = table["sql"].as_str().unwrap().replace(
            &format!("CREATE TABLE \"{}\"", name.replace('"', "\"\"")),
            &format!("CREATE TABLE {name}"),
        );
        table["sql"] = json!(sql);
    }
    value
}

#[tokio::test]
async fn actual_source_download_jobs_preserve_archive_lifecycle() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/postprocessors/archive_jobs.json")).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 21);
    assert_eq!(
        fixture["generator_source"],
        include_str!("../fixtures/postprocessors/generate_archive_jobs.py")
    );
    let client = download::client(Duration::from_secs(2)).unwrap();
    let stdout_only = std::env::var_os("GDL_ARCHIVE_STDOUT_REPLAY").is_some();
    let mut covered = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        if stdout_only && name != "stdout-initializes-archive-but-bypasses-check-add" {
            continue;
        }
        let root = tempfile::tempdir().unwrap();
        for (path, data) in case["preseed"].as_object().unwrap() {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data.as_str().unwrap()).unwrap();
        }
        for (job_index, job) in case["jobs"].as_array().unwrap().iter().enumerate() {
            covered += 1;
            let input = &job["input"];
            let phase = job["phase"].as_str().unwrap();
            let observer = format!("observer-{job_index}.json");
            let mut options = input["postprocessors"].as_array().unwrap().clone();
            for option in &mut options {
                option["base-directory"] = json!(root.path());
                if let Some(path) = option["archive"].as_str() {
                    option["archive"] = json!(root.path().join(path));
                }
            }
            options.push(json!({"name":"metadata","event":"finalize","mode":"json","filename":observer,"base-directory":root.path(),"include":KEYS}));
            let pp = Arc::new(Postprocessors::new(
                options
                    .iter()
                    .map(|v| Specification::parse(v, root.path()).unwrap())
                    .collect(),
            ));
            let filename =
                FilenameTemplate::parse(&format!("file-{phase}-{{id}}-{{num}}.{{extension}}"))
                    .unwrap();
            let directory = DirectoryTemplate::parse(&[]).unwrap();
            let configs = Arc::new(BTreeMap::from([(
                "fixture".into(),
                JobConfiguration {
                    processors: pp.clone(),
                    root: root.path().to_owned(),
                    filename: Some(filename.clone()),
                    directory: Some(directory.clone()),
                    init: input["init"] == true,
                },
            )]));
            let mut stream = jobs::apply_with_descriptor(
                stream::iter(source_events(input, phase)).boxed(),
                configs,
                Some("fixture".into()),
                Some(descriptor()),
            );
            let mut failed = false;
            let mut emitted = 0;
            while let Some(item) = stream.next().await {
                let item = item.unwrap_or_else(|e| panic!("{name} job {job_index}: {e}"));
                emitted += 1;
                let outcome = download::download_with_template(
                    &client,
                    root.path(),
                    &item.media,
                    DownloadOptions {
                        postprocessors: Some(&pp),
                        postprocessor_scope: item.scope.as_ref(),
                        directory: Some(&directory),
                        ..Default::default()
                    },
                    Some(&filename),
                )
                .await;
                if outcome.is_err() {
                    failed = true;
                    item.scope.as_ref().unwrap().fail();
                }
                drop(item);
            }
            assert_eq!(failed, job["status"] != 0, "{name} job {job_index}");
            assert_eq!(
                emitted,
                if failed { 1 } else { 2 },
                "{name}: abort callback processing immediately"
            );
            let mut actual: Value =
                serde_json::from_slice(&fs::read(root.path().join(&observer)).unwrap()).unwrap();
            normalize(&mut actual, root.path());
            // Include filters deliberately check absent fields as well as values.
            let actual: Map<String, Value> = actual
                .as_object()
                .unwrap()
                .iter()
                .filter(|(k, _)| KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(
                Value::Object(actual),
                job["final_keywords"],
                "{name} job {job_index}: final shared source dictionary"
            );
            for artifact in job["artifacts"].as_array().unwrap() {
                let path = artifact["path"].as_str().unwrap();
                let physical = root.path().join(path);
                if artifact["kind"] == "sqlite" {
                    assert!(physical.exists(), "{name}: missing archive {path}");
                    assert_eq!(
                        tables(&physical),
                        normalized_tables(artifact["tables"].clone()),
                        "{name} job {job_index}: {path}"
                    );
                } else if failed && (path.ends_with(".part") || path == "meta-1.txt") {
                    // Encoding errors never publish a new metadata file. A
                    // previous successful job's bytes survive atomic rollback.
                    if path == "meta-1.txt" {
                        if job_index == 0 {
                            assert!(!physical.exists(), "{name}");
                        } else {
                            assert_eq!(fs::read(&physical).unwrap(), b"1:1:first", "{name}");
                        }
                    }
                } else {
                    let expected = artifact
                        .get("utf8")
                        .map(|v| v.as_str().unwrap().as_bytes().to_vec())
                        .unwrap_or_else(|| {
                            let hex = artifact["hex"].as_str().unwrap();
                            (0..hex.len())
                                .step_by(2)
                                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                                .collect()
                        });
                    assert_eq!(
                        fs::read(&physical).unwrap_or_else(|e| panic!("{name}: {path}: {e}")),
                        expected,
                        "{name} job {job_index}: {path}"
                    );
                }
            }
            for sidecar in ["meta-1.txt", "meta-2.txt", "second-1.txt", "second-2.txt"] {
                let expected = job["artifacts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["path"] == sidecar);
                let expected = if failed && sidecar == "meta-1.txt" {
                    job_index > 0
                } else {
                    expected
                };
                assert_eq!(
                    root.path().join(sidecar).exists(),
                    expected,
                    "{name} job {job_index}: sidecar presence {sidecar}"
                );
            }
            // These modes initialize SQLite yet bypass archive check/add, key
            // caching, and metadata-path mutation. stdout is exercised as real
            // stdout above; the source oracle also pins its exact rendered text.
            if input["postprocessors"][0]["filename"] == "-" {
                assert_eq!(job["stdout"], "1:1:first1:2:second");
                assert_eq!(
                    tables(&root.path().join("archive.sqlite3"))[0]["rows"],
                    json!([])
                );
                assert!(!root.path().join("meta-1.txt").exists());
            }
        }
    }
    assert_eq!(
        covered,
        if stdout_only { 1 } else { 22 },
        "Every source job must be replayed, including the later failure and stdout modes"
    );
    if !stdout_only {
        // Tokio stdout bypasses libtest's thread-local print capture. Replay
        // this real mode in a subprocess and compare the actual emitted bytes.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "postprocessor_archives::actual_source_download_jobs_preserve_archive_lifecycle",
                "--nocapture",
            ])
            .env("GDL_ARCHIVE_STDOUT_REPLAY", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout replay failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = b"1:1:first1:2:second";
        assert_eq!(
            output
                .stdout
                .windows(expected.len())
                .filter(|v| *v == expected)
                .count(),
            1,
            "stdout mode must emit the exact two concatenated source records: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
