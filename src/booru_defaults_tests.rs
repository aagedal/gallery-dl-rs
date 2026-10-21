//! Published-path replay against actual source DownloadJob/PathFormat fixtures.
use super::{
    tests::{booru, response},
    *,
};
use crate::{
    archive::{Archive, ArchiveOutcome},
    config::Config,
    download::{self, DownloadOptions},
    naming::{DirectoryTemplate, FilenameTemplate},
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration, JobMedia},
    },
};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

const EVENTS: &[&str] = &["post", "prepare", "file", "after", "post-after"];
const KEYS: &[&str] = &[
    "category",
    "id",
    "filename",
    "extension",
    "md5",
    "search_tags",
    "pool",
    "favgroup",
];
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_defaults.json"
    ))
    .unwrap()
}
pub(crate) fn serve(replies: Vec<Vec<u8>>) -> (Url, std::thread::JoinHandle<Vec<String>>) {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Instant,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "missing API request after {}",
                            requests.len()
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|v| v == b"\r\n\r\n") {
                let mut bytes = [0; 8192];
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&bytes[..n]);
                assert!(request.len() <= 64 * 1024);
            }
            requests.push(String::from_utf8(request).unwrap());
            let _ = socket.write_all(&reply);
        }
        requests
    });
    (root, task)
}
fn templates(case: &Value, root: &Path) -> (Option<FilenameTemplate>, Option<DirectoryTemplate>) {
    let family = case["family"].as_str().unwrap();
    let cfg = Config::parse(
        &serde_json::to_vec(&json!({"extractor":{family:case["overrides"]}})).unwrap(),
        root,
    )
    .unwrap();
    let options = cfg.resolve(family).unwrap();
    let filename = match &options.filename_conditions {
        Some(conditions) => Some(
            FilenameTemplate::parse_conditional(conditions, options.filename.as_deref()).unwrap(),
        ),
        None => options
            .filename
            .as_deref()
            .map(|v| FilenameTemplate::parse(v).unwrap()),
    };
    let directory = match &options.directory_conditions {
        Some(conditions) => Some(
            DirectoryTemplate::parse_conditional(conditions, options.directory.as_deref()).unwrap(),
        ),
        None => options
            .directory
            .as_deref()
            .map(|v| DirectoryTemplate::parse(v).unwrap()),
    };
    (filename, directory)
}
pub(crate) fn replies(case: &Value, site: Site, source: &Url) -> Vec<Vec<u8>> {
    let target = target(site, source).unwrap().0;
    let posts = case["input_posts"].as_array().unwrap();
    let body = match (&target, site) {
        (Target::Post { .. }, Site::Danbooru) => posts[0].clone(),
        (Target::Post { .. }, Site::E621) => json!({"post":posts[0]}),
        (_, Site::E621) => json!({"posts":posts}),
        (_, Site::Gelbooru) => json!({"post":posts}),
        _ => json!(posts),
    };
    let mut replies = Vec::new();
    if matches!(target, Target::Pool { .. } | Target::Favgroup { .. }) {
        replies.push(response(
            "200 OK",
            "",
            case["collection_response"].to_string().as_bytes(),
        ));
    }
    replies.push(response("200 OK", "", body.to_string().as_bytes()));
    replies
}
fn artifacts(root: &Path) -> Value {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<Value>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                if EVENTS
                    .iter()
                    .any(|event| relative == format!("oracle-{event}.jsonl"))
                    || matches!(
                        relative.as_str(),
                        "history.sqlite3"
                            | "history.sqlite3-wal"
                            | "history.sqlite3-shm"
                            | "history.sqlite3-journal"
                    )
                {
                    continue;
                }
                let bytes = std::fs::read(&path).unwrap();
                files.push(json!({"path":relative,"data_hex":bytes.iter().map(|b|format!("{b:02x}")).collect::<String>()}));
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    json!(files)
}

async fn replay(case: &Value, archived: bool, scoped: bool) {
    let name = case["name"].as_str().unwrap();
    assert!(case["error"].is_null(), "{name}: source must succeed");
    let family = case["family"].as_str().unwrap();
    let site = Site::from_category(family).unwrap();
    let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
    let (api_root, server) = serve(replies(case, site, &source));
    let root = tempfile::tempdir().unwrap();
    let (filename, directory) = templates(case, root.path());
    let mut keys = KEYS.to_vec();
    if !case["collection_response"].is_null() {
        keys.push("num");
    }
    let mut specifications = case["hook_options"].as_array().unwrap().clone();
    if scoped {
        for event in EVENTS {
            specifications.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root.path(),"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":keys}));
        }
    }
    let processors = Arc::new(Postprocessors::new(
        specifications
            .iter()
            .map(|v| Specification::parse(v, root.path()).unwrap())
            .collect(),
    ));
    let client = booru(site, api_root);
    let events = client
        .events(&source)
        .map(|event| {
            event.map(|mut event| {
                if let ExtractionEvent::Media(media) = &mut event {
                    (media.url, media.job) =
                        crate::extract::inline_text("source-default-path-fixture".into()).unwrap();
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
    let mut output = if scoped {
        jobs::apply_with_descriptor(
            events,
            Arc::new(BTreeMap::from([(family.into(), configuration)])),
            Some(family.into()),
            crate::source_archive::for_url(&source),
        )
    } else {
        crate::event::media_only(events)
            .map(|result| result.map(|media| JobMedia { media, scope: None }))
            .boxed()
    };
    let download = download::client(Duration::from_secs(2)).unwrap();
    let archive = if archived {
        Some(
            Archive::open(root.path().join("history.sqlite3"))
                .await
                .unwrap(),
        )
    } else {
        None
    };

    let source_posts = case["trace"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["event"] == "post")
        .collect::<Vec<_>>();
    let mut index = 0;
    while let Some(item) = output.next().await {
        let item =
            item.unwrap_or_else(|e| panic!("{name}, archive{archived}, scoped{scoped}: {e}"));
        if let Some(scope) = &item.scope {
            let expected = source_posts[index]["directory"]
                .as_str()
                .unwrap()
                .trim_end_matches('/');
            assert_eq!(
                scope.directory().unwrap(),
                root.path().join(expected),
                "{name}: pre-Post directory checkpoint"
            );
        }
        let options = DownloadOptions {
            postprocessors: scoped.then_some(processors.as_ref()),
            postprocessor_scope: item.scope.as_ref(),
            directory: directory.as_ref(),
            ..Default::default()
        };
        let outcome = if let Some(archive) = &archive {
            let result = archive
                .download_with_template(
                    &download,
                    root.path(),
                    &item.media,
                    options,
                    filename.as_ref(),
                )
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let ArchiveOutcome::Completed(outcome) = result else {
                panic!("{name}: unexpected archive hit");
            };
            assert!(
                archive
                    .contains(&item.media.archive_key().unwrap())
                    .await
                    .unwrap()
            );
            outcome
        } else {
            download::download_with_template(
                &download,
                root.path(),
                &item.media,
                options,
                filename.as_ref(),
            )
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        let path = outcome.saved_path().unwrap();
        assert!(path.is_file(), "{name}: published output must exist");
        index += 1;
    }
    assert_eq!(
        artifacts(root.path()),
        case["published"],
        "{name}: entire published paths, archive{archived}, scoped{scoped}"
    );
    if scoped {
        for event in EVENTS {
            let expected = case["trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["event"] == *event)
                .collect::<Vec<_>>();
            let text = std::fs::read_to_string(root.path().join(format!("oracle-{event}.jsonl")))
                .unwrap_or_else(|e| panic!("{name}: {event}: {e}"));
            let actual = text
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual.len(), expected.len(), "{name}: {event} hook count");
            for (actual, expected) in actual.iter().zip(expected) {
                for key in &keys {
                    assert_eq!(
                        actual.get(*key),
                        expected["metadata"].get(*key),
                        "{name}: {event} keyword {key}, archive{archived}"
                    );
                }
            }
        }
    }
    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        if case["collection_response"].is_null() {
            1
        } else {
            2
        },
        "{name}: only expected API requests"
    );
}
#[tokio::test]
async fn real_source_default_paths_and_hook_keywords_match_scoped_downloads_and_archives() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 174);
    for case in cases {
        for archived in [false, true] {
            replay(case, archived, true).await;
        }
    }
}
#[tokio::test]
async fn source_default_paths_apply_without_job_hooks_or_postprocessors() {
    let fixture = fixture();
    let mut count = 0;
    for case in fixture["cases"].as_array().unwrap() {
        if case["hook_options"].as_array().unwrap().is_empty() {
            for archived in [false, true] {
                replay(case, archived, false).await;
            }
            count += 1;
        }
    }
    assert_eq!(count, 122);
}
