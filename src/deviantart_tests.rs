use super::*;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    thread::{self, JoinHandle},
};

const UUID: &str = "1A2B3C4D-0000-1111-2222-333344445555";

fn uuid(n: u8) -> String {
    format!("00000000-0000-0000-0000-0000000000{n:02}")
}

fn deviation(id: &str, index: u64, downloadable: bool) -> Value {
    json!({"deviationid": id, "title": "My Art", "published_time": "1704067200",
        "url": format!("https://www.deviantart.com/alice/art/My-Art-{index}"),
        "is_downloadable": downloadable, "is_deleted": false,
        "author": {"username": "Alice"},
        "content": {"src": "https://images-wixmp-abc.wixmp.com/f/a/b/v1/fill/w_800,q_75,blur_5/my_art.jpg?token=secret",
            "width": 800, "height": 600}})
}

fn response_with(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn response(status: &str, body: &[u8]) -> Vec<u8> {
    response_with(status, "", body)
}

fn ok(value: Value) -> Vec<u8> {
    response("200 OK", value.to_string().as_bytes())
}

fn html(text: &str) -> Vec<u8> {
    response("200 OK", text.as_bytes())
}

fn token(value: &str) -> Vec<u8> {
    ok(json!({"access_token": value, "token_type": "Bearer"}))
}

fn profile(name: &str) -> Vec<u8> {
    ok(json!({"user": {"username": name, "userid": "X",
        "usericon": "https://a.deviantart.net/avatars/a/l/alice.png?3"},
        "cover_deviation": {"cover_deviation": deviation(&uuid(7), 900000007, false)}}))
}

fn page(results: Vec<Value>, next: Option<u64>) -> Vec<u8> {
    ok(json!({"results": results, "has_more": next.is_some(), "next_offset": next}))
}

fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            requests.push(String::from_utf8(request).unwrap());
            let _ = stream.write_all(&reply);
        }
        requests
    });
    (root, task)
}

fn client(root: Url) -> DeviantArt {
    DeviantArt::new(Duration::from_secs(5))
        .unwrap()
        .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
        .unwrap()
        .with_root(root)
}

fn with(root: Url, options: Value) -> DeviantArt {
    client(root)
        .with_options(Some(options.as_object().unwrap()))
        .unwrap()
}

async fn run(client: &DeviantArt, url: &str) -> Vec<ExtractionEvent> {
    client
        .events(&Url::parse(url).unwrap())
        .map(|event| event.unwrap())
        .collect()
        .await
}

fn files(events: &[ExtractionEvent]) -> Vec<&Media> {
    events
        .iter()
        .filter_map(|event| match event {
            ExtractionEvent::Media(media) => Some(&**media),
            _ => None,
        })
        .collect()
}

fn children(events: &[ExtractionEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, ExtractionEvent::BeginChild(_)))
        .count()
}

fn keywords(media: &Media) -> &Map<String, Value> {
    &media
        .metadata
        .as_ref()
        .unwrap()
        .site_keywords
        .as_ref()
        .unwrap()
        .fields
}

fn paths(media: &Media) -> (String, PathBuf) {
    let (file, dir) = crate::sites::default_templates(media).unwrap();
    (
        file.render(media).unwrap(),
        dir.render(media).unwrap().unwrap(),
    )
}

fn dir(parts: &[&str]) -> PathBuf {
    parts.iter().collect()
}

fn legacy_key(identity: Value) -> String {
    format!(
        "deviantart:v1:{:x}",
        Sha256::digest(identity.to_string().as_bytes())
    )
}

fn line(request: &str) -> &str {
    request.lines().next().unwrap()
}

#[test]
fn classifies_every_upstream_class() {
    let t = |raw: &str| target(&Url::parse(raw).unwrap());
    let user = |s: &str| s.to_owned();
    let art = Target::Deviation {
        user: Some(user("alice")),
        kind: "art",
        id: "123".into(),
    };
    for raw in [
        "https://www.deviantart.com/alice/art/My-Art-123",
        "https://alice.deviantart.com/art/My-Art-123",
        "https://www.fxdeviantart.com/Alice/art/123",
        "https://alice.fxdeviantart.com/art/My-Art-123?x=1",
    ] {
        assert_eq!(t(raw).unwrap(), art, "{raw}");
    }
    assert_eq!(
        t("https://www.deviantart.com/alice/journal/Notes-9").unwrap(),
        Target::Deviation {
            user: Some(user("alice")),
            kind: "journal",
            id: "9".into()
        }
    );
    let bare = |id: &str| Target::Deviation {
        user: None,
        kind: "art",
        id: id.into(),
    };
    assert_eq!(t("https://www.deviantart.com/view/42").unwrap(), bare("42"));
    assert_eq!(
        t("https://deviantart.com/deviation/42").unwrap(),
        bare("42")
    );
    assert_eq!(
        t("https://www.deviantart.com/view.php?id=42").unwrap(),
        bare("42")
    );
    assert_eq!(t("https://fav.me/d16").unwrap(), bare("42"));
    let cases = [
        (
            "https://www.deviantart.com/alice",
            Target::User(user("alice")),
        ),
        ("https://alice.deviantart.com/", Target::User(user("alice"))),
        (
            "https://deviantart.com/alice/gallery",
            Target::Gallery(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/gallery/all",
            Target::Gallery(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/gallery/7/My-Folder",
            Target::Folder {
                user: user("alice"),
                id: "7".into(),
                name: Some("My-Folder".into()),
            },
        ),
        (
            "https://www.deviantart.com/alice/gallery/scraps",
            Target::Scraps(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/gallery/?catpath=scraps",
            Target::Scraps(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/gallery?q=cat&sort=popular",
            Target::GallerySearch {
                user: user("alice"),
                query: "q=cat&sort=popular".into(),
            },
        ),
        (
            "https://www.deviantart.com/alice/favourites",
            Target::Favorite(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/favourites/all",
            Target::Favorite(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/favourites/8/Best-Of",
            Target::Collection {
                user: user("alice"),
                id: "8".into(),
                name: "Best-Of".into(),
            },
        ),
        (
            "https://www.deviantart.com/alice/posts/journals/",
            Target::Journal(user("alice")),
        ),
        (
            "https://alice.deviantart.com/journal/",
            Target::Journal(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/posts/statuses",
            Target::Status(user("alice")),
        ),
        (
            "https://www.deviantart.com/Alice/avatar/",
            Target::Avatar(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/banner",
            Target::Background(user("alice")),
        ),
        (
            "https://www.deviantart.com/tag/cute%20cats",
            Target::Tag("cute cats".into()),
        ),
        (
            "https://www.deviantart.com/search?q=cats&order=popular",
            Target::Search("q=cats&order=popular".into()),
        ),
        (
            "https://www.deviantart.com/search/deviations?q=cats",
            Target::Search("q=cats".into()),
        ),
        (
            "https://www.deviantart.com/stash/0abc123",
            Target::Stash("0abc123".into()),
        ),
        ("https://sta.sh/0abc123", Target::Stash("0abc123".into())),
        (
            "https://www.deviantart.com/Alice/about#watching",
            Target::Following(user("alice")),
        ),
        (
            "https://www.deviantart.com/alice/watching",
            Target::Following(user("alice")),
        ),
        (
            "https://alice.deviantart.com/watching/",
            Target::Following(user("alice")),
        ),
        ("https://www.deviantart.com/watch/deviations", Target::Watch),
        ("https://deviantart.com/notifications/watch", Target::Watch),
        ("https://www.deviantart.com/watch/posts", Target::WatchPosts),
    ];
    for (raw, expected) in cases {
        assert_eq!(t(raw).unwrap(), expected, "{raw}");
        assert!(is_deviantart_url(&Url::parse(raw).unwrap()), "{raw}");
    }
    for raw in [
        "https://www.deviantart.com/",
        "https://www.deviantart.com/tag/",
        "https://www.deviantart.com/search",
        "https://www.deviantart.com/watch/other",
        "https://www.fxdeviantart.com/watch/deviations",
        "https://www.deviantart.com/alice/about",
        "https://www.deviantart.com/alice/about#gallery",
        "https://www.deviantart.com/alice/art/no-id",
        "https://www.deviantart.com/alice/gallery/7",
        "https://www.deviantart.com/alice/unknown",
        "https://www.deviantart.com:444/alice",
        "https://a.b.deviantart.com/",
        "https://www.deviantart.com.evil.example/alice",
        "https://deviantart.com.evil.example/tag/x",
        "https://evil-deviantart.com/alice",
        "https://fav.me.evil.example/d16",
        "https://sta.sh.evil.example/0abc",
        "https://user:pw@www.deviantart.com/alice",
        "ftp://www.deviantart.com/alice",
    ] {
        assert!(t(raw).is_err(), "{raw}");
    }
}

#[test]
fn rewrites_urls_and_parses_helpers_like_upstream() {
    let src = "https://images-wixmp-x.wixmp.com/f/abc/def/v1/fill/w_800,q_75,blur_5/a.jpg?token=t";
    assert_eq!(
        intermediary(src).as_deref(),
        Some("https://images-wixmp-x.wixmp.com/intermediary/f/abc/def")
    );
    assert_eq!(intermediary("https://x/f/abc/def.jpg"), None);
    assert_eq!(
        replace_digits(src, ",q_", ",q_100"),
        src.replace(",q_75", ",q_100")
    );
    assert_eq!(
        replace_digits(src, ",blur_", ""),
        src.replace(",blur_5", "")
    );
    assert_eq!(
        png_quality("https://x/a-fullview.jpg?token=t"),
        "https://x/a-fullview.png?token=t"
    );
    assert_eq!((b36encode(123), b36decode("3f")), ("3f".into(), Some(123)));
    assert_eq!((b36encode(0), b36decode("")), (String::new(), Some(0)));
    assert_eq!(slug_id("My-Art-123"), Some("123"));
    assert_eq!(slug_id("a-1b-c"), Some("1"));
    assert_eq!(slug_id("456"), Some("456"));
    assert_eq!(slug_id("no-id"), None);
    assert!(folder_name_matches("My-Folder", "My Folder"));
    assert!(folder_name_matches("my-folder", "MY -- FOLDER"));
    assert!(!folder_name_matches("my-folder", "my folder 2"));
    assert!(!folder_name_matches("my-folder", "myfolder"));
    assert_eq!(word_chars("my art: v2!"), "my_art__v2_");
    assert_eq!(escape("<a & 'b'>"), "&lt;a &amp; &#x27;b&#x27;&gt;");
    let links = search_links(
        r#"<a href="https://www.deviantart.com/bob/art/Cat-11"><a href="https://www.deviantart.com/eve/journal/22"><a href="https://www.deviantart.com/x/about">"#,
    );
    assert_eq!(
        links,
        vec![
            ("bob".to_owned(), false, "11".to_owned()),
            ("eve".to_owned(), true, "22".to_owned())
        ]
    );
    let html = format!(r#"x\"deviationUuid\":\"{UUID}\"y"#);
    assert_eq!(page_uuid(&html).as_deref(), Some(UUID));
    let app = format!(r#"<meta property="da:appurl" content="DeviantArt://deviation/{UUID}"/>"#);
    assert_eq!(page_uuid(&app).as_deref(), Some(UUID));
    assert_eq!(page_uuid("DeviantArt://deviation/not-a-uuid"), None);
    let post = |post: &str, media: &str| PostMetadata {
        post_id: post.into(),
        media_id: media.into(),
        ..PostMetadata::default()
    };
    assert_eq!(
        archive_identity(&post(UUID, UUID)),
        Some(json!([UUID]).to_string())
    );
    assert_eq!(
        archive_identity(&post(UUID, &format!("{UUID}:journal"))),
        Some(json!([UUID, "journal"]).to_string())
    );
    assert_eq!(archive_identity(&post(UUID, &format!("{UUID}:x"))), None);
    assert_eq!(
        archive_identity(&post("alice", "avatar:3")),
        Some(json!(["avatar", "alice", "3"]).to_string())
    );
    assert_eq!(archive_identity(&post("a/b", "avatar:3")), None);
    assert_eq!(archive_identity(&post("123", "123")), None);
}

#[test]
fn parses_options_and_rejects_invalid_values() {
    let parse = |v: Value| Options::parse(v.as_object().unwrap());
    let o = parse(json!({})).unwrap();
    assert_eq!(o.include, vec!["gallery"]);
    assert_eq!(
        (o.journals, o.original, o.quality.as_deref()),
        (Journals::Html, Original::Off, Some(",q_100"))
    );
    let o = parse(
        json!({"include": "favorite, journal,bogus", "journals": "none",
        "original": "images", "quality": "png", "metadata": "all", "group": "skip",
        "folder": {"subfolders": false}, "avatar": {"formats": ["big.jpg", ".png"]}}),
    )
    .unwrap();
    assert_eq!(o.include, vec!["favorite", "journal"]);
    assert_eq!((o.journals, o.original), (Journals::Off, Original::Images));
    assert_eq!(o.quality.as_deref(), Some("-fullview.png?"));
    assert_eq!(o.metadata.as_ref().unwrap().len(), 5);
    assert_eq!(o.metadata_limit(), Some(10));
    assert_eq!((o.group, o.subfolders), (Group::Skip, false));
    assert_eq!(o.avatar_formats.unwrap(), vec!["big.jpg", ".png"]);
    assert_eq!(parse(json!({"include": "all"})).unwrap().include, INCLUDE);
    let o = parse(json!({"quality": 80, "original": true, "metadata": true})).unwrap();
    assert_eq!(
        (o.quality.as_deref(), o.original, o.metadata_limit()),
        (Some(",q_80"), Original::On, Some(50))
    );
    assert_eq!(parse(json!({"quality": false})).unwrap().quality, None);
    for bad in [
        json!({"flat": "yes"}),
        json!({"quality": "9/../x"}),
        json!({"metadata": ["a&b"]}),
        json!({"journals": 1}),
        json!({"avatar": {"formats": ["../x"]}}),
        json!({"include": 5}),
    ] {
        assert!(parse(bad.clone()).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn single_deviation_uses_upstream_keywords_paths_and_original() {
    let page = format!(r#"<script>x\"deviationUuid\":\"{UUID}\",y</script>"#);
    let (root, server) = serve(vec![
        token("first"),
        response("401 Unauthorized", b"{}"),
        token("second"),
        profile("Alice"),
        html(&page),
        response("503 Service Unavailable", b""),
        ok(deviation(UUID, 123, true)),
        ok(
            json!({"src": "https://wixmp-x.wixmp.com/d/orig.png?token=t", "width": 4000,
            "height": 3000, "filename": "my_art_by_alice.png"}),
        ),
    ]);
    let client = with(root, json!({"original": true}))
        .with_retries(1)
        .with_credentials(Some("42"), Some("topsecret"));
    let events = run(&client, "https://alice.deviantart.com/art/My-Art-123").await;
    let media = files(&events);
    assert_eq!(media.len(), 1);
    let file = media[0];
    assert_eq!(
        file.url.as_str(),
        "https://wixmp-x.wixmp.com/d/orig.png?token=t"
    );
    assert!(file.headers.is_empty());
    let k = keywords(file);
    assert_eq!(
        (&k["category"], &k["subcategory"]),
        (&json!("deviantart"), &json!("deviation"))
    );
    assert_eq!(
        (&k["index"], &k["index_base36"]),
        (&json!(123), &json!("3f"))
    );
    assert_eq!(
        (&k["username"], &k["_username"]),
        (&json!("Alice"), &json!("alice"))
    );
    assert_eq!(k["filename"], "my_art_by_alice-d3f");
    assert_eq!(
        (&k["extension"], &k["is_original"]),
        (&json!("png"), &json!(true))
    );
    assert_eq!(
        (&k["published_time"], &k["date"]),
        (&json!(1704067200), &json!("2024-01-01 00:00:00"))
    );
    assert_eq!(
        (&k["num"], &k["count"], &k["index_file"]),
        (&json!(1), &json!(1), &json!(0))
    );
    assert_eq!(k["target"]["filename"], "my_art_by_alice-d3f");
    assert!(!k.contains_key("_page"));
    assert_eq!(
        paths(file),
        (
            "deviantart_123_My Art.png".into(),
            dir(&["deviantart", "Alice"])
        )
    );
    let meta = file.metadata.as_ref().unwrap();
    assert_eq!((meta.post_id.as_str(), meta.index), (UUID, 1));
    assert_eq!(meta.file.as_ref().unwrap().width, Some(4000));
    // Archive identity is unchanged from the earlier UUID-only scheme.
    assert_eq!(file.archive_key().unwrap(), legacy_key(json!([UUID])));
    let requests = server.join().unwrap();
    assert!(line(&requests[0]).starts_with("POST /oauth2/token "));
    assert!(
        requests[0].contains("grant_type=client_credentials&client_id=42&client_secret=topsecret")
    );
    assert!(
        line(&requests[1])
            .starts_with("GET /api/v1/oauth2/user/profile/alice?mature_content=true ")
    );
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer first")
    );
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("da-minor-version: 20210526")
    );
    assert!(
        requests[3]
            .to_ascii_lowercase()
            .contains("authorization: bearer second")
    );
    assert!(line(&requests[4]).starts_with("GET /Alice/art/123 "));
    assert!(!requests[4].to_ascii_lowercase().contains("authorization"));
    let detail = format!("GET /api/v1/oauth2/deviation/{UUID}?mature_content=true ");
    assert!(line(&requests[6]).starts_with(&detail));
    assert!(line(&requests[7]).starts_with(&format!(
        "GET /api/v1/oauth2/deviation/download/{UUID}?mature_content=true "
    )));
}

#[tokio::test]
async fn previews_by_default_with_quality_and_intermediary_rewriting() {
    let page = format!(r#"<meta content="DeviantArt://deviation/{UUID}">"#);
    let (root, server) = serve(vec![
        html(&page),
        token("t"),
        ok(deviation(UUID, 123, true)),
    ]);
    let client = with(root, json!({"group": false}));
    let events = run(&client, "https://www.deviantart.com/view/123").await;
    let file = files(&events)[0];
    // Older deviations use `/intermediary/`, keeping the preview as fallback.
    assert_eq!(
        file.url.as_str(),
        "https://images-wixmp-abc.wixmp.com/intermediary/f/a/b"
    );
    assert_eq!(keywords(file)["is_original"], false);
    assert_eq!(keywords(file)["username"], "Alice");
    assert_eq!(
        file.handoff["_fallback"][0],
        deviation(UUID, 1, false)["content"]["src"]
    );
    assert_eq!(server.join().unwrap().len(), 3);

    let (root, _server) = serve(vec![
        html(&page),
        token("t"),
        ok(deviation(UUID, 900000000, true)),
    ]);
    let client = with(root, json!({"group": false, "quality": 90}));
    let events = run(&client, "https://www.deviantart.com/view/900000000").await;
    let file = files(&events)[0];
    assert_eq!(
        file.url.as_str(),
        "https://images-wixmp-abc.wixmp.com/f/a/b/v1/fill/w_800,q_90/my_art.jpg?token=secret"
    );
    assert_eq!(keywords(file)["is_original"], false);
}

#[tokio::test]
async fn paginates_galleries_folders_and_queues_subfolders() {
    let folders = json!({"results": [{"folderid": uuid(9), "name": "Featured", "parent": null},
        {"folderid": uuid(8), "name": "My Folder", "parent": uuid(9), "has_subfolders": true,
         "subfolders": [{"folderid": uuid(6), "name": "Sub", "parent": uuid(8)}]}],
        "has_more": false});
    let (root, server) = serve(vec![
        token("t"),
        profile("Alice"),
        ok(folders),
        page(vec![deviation(&uuid(5), 900000005, false)], None),
        page(
            vec![
                deviation(&uuid(1), 900000001, false),
                json!({"deviationid": uuid(2), "title": "deleted", "is_deleted": true}),
            ],
            Some(24),
        ),
        page(vec![deviation(&uuid(3), 900000003, false)], None),
    ]);
    let client = client(root);
    let events = run(
        &client,
        "https://www.deviantart.com/alice/gallery/5/my-folder",
    )
    .await;
    let media = files(&events);
    assert_eq!(media.len(), 3);
    assert_eq!(children(&events), 1);
    let sub = keywords(media[0]);
    assert_eq!(sub["folder"]["parent_folder"], "My Folder");
    assert_eq!(
        paths(media[0]).1,
        dir(&["deviantart", "Alice", "My Folder"])
    );
    let k = keywords(media[1]);
    assert_eq!(k["subcategory"], "folder");
    assert_eq!(
        k["folder"],
        json!({"title": "My Folder", "uuid": uuid(8), "index": "5", "owner": "Alice",
            "parent_uuid": uuid(9)})
    );
    assert_eq!(
        paths(media[1]),
        (
            "deviantart_900000001_My Art.jpg".into(),
            dir(&["deviantart", "Alice", "My Folder"])
        )
    );
    assert_eq!(
        media[1].archive_key().unwrap(),
        legacy_key(json!([uuid(1)]))
    );
    let requests = server.join().unwrap();
    assert!(line(&requests[2]).starts_with(
        "GET /api/v1/oauth2/gallery/folders?mature_content=true&username=Alice&offset=0&limit=50 "
    ));
    assert!(line(&requests[3]).starts_with(&format!(
        "GET /api/v1/oauth2/gallery/{}?mature_content=true&username=Alice&mode=newest&offset=0&limit=24 ",
        uuid(6)
    )));
    let folder = format!(
        "GET /api/v1/oauth2/gallery/{}?mature_content=true&username=Alice&mode=newest",
        uuid(8)
    );
    assert!(line(&requests[4]).starts_with(&format!("{folder}&offset=0&limit=24 ")));
    assert!(line(&requests[5]).starts_with(&format!("{folder}&offset=24&limit=24 ")));

    let (root, server) = serve(vec![
        token("t"),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let client = with(root, json!({"group": false}));
    let events = run(&client, "https://www.deviantart.com/alice/gallery").await;
    let media = files(&events);
    assert_eq!(keywords(media[0])["subcategory"], "gallery");
    assert_eq!(keywords(media[0])["username"], "alice");
    assert_eq!(paths(media[0]).1, dir(&["deviantart", "alice"]));
    let requests = server.join().unwrap();
    assert!(line(&requests[1]).starts_with(
        "GET /api/v1/oauth2/gallery/all?mature_content=true&username=alice&offset=0&limit=24 "
    ));
}

#[tokio::test]
async fn favorites_collections_and_non_flat_dispatch() {
    let (root, server) = serve(vec![
        token("t"),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let client = with(root, json!({"group": false}));
    let events = run(&client, "https://www.deviantart.com/alice/favourites").await;
    let file = files(&events)[0];
    assert_eq!(keywords(file)["subcategory"], "favorite");
    assert_eq!(paths(file).1, dir(&["deviantart", "alice", "Favourites"]));
    assert_eq!(file.archive_key().unwrap(), legacy_key(json!([uuid(1)])));
    assert!(line(&server.join().unwrap()[1]).starts_with(
        "GET /api/v1/oauth2/collections/all?mature_content=true&username=alice&offset=0&limit=24 "
    ));

    let collections = json!({"results": [{"folderid": uuid(4), "name": "Best: Of"}],
        "has_more": false});
    let (root, server) = serve(vec![
        token("t"),
        ok(collections.clone()),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let client = with(root, json!({"group": false}));
    let events = run(
        &client,
        "https://www.deviantart.com/alice/favourites/8/best-of",
    )
    .await;
    let file = files(&events)[0];
    let k = keywords(file);
    assert_eq!(k["subcategory"], "collection");
    assert_eq!(
        k["collection"],
        json!({"title": "Best: Of", "uuid": uuid(4), "index": "8", "owner": "alice"})
    );
    let title = if cfg!(windows) {
        "Best_ Of"
    } else {
        "Best: Of"
    };
    assert_eq!(
        paths(file).1,
        dir(&["deviantart", "alice", "Favourites", title])
    );
    let requests = server.join().unwrap();
    assert!(line(&requests[2]).starts_with(&format!(
        "GET /api/v1/oauth2/collections/{}?mature_content=true&username=alice&offset=0&limit=24 ",
        uuid(4)
    )));

    // `flat: false` queues each collection as its own child job.
    let (root, server) = serve(vec![
        token("t"),
        ok(collections),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let client = with(root, json!({"group": false, "flat": false}));
    let events = run(&client, "https://www.deviantart.com/alice/favourites").await;
    assert_eq!((children(&events), files(&events).len()), (1, 1));
    assert_eq!(keywords(files(&events)[0])["subcategory"], "collection");
    assert_eq!(server.join().unwrap().len(), 3);
}

#[tokio::test]
async fn scraps_use_eclipse_api_with_session_cookie() {
    let (root, server) = serve(vec![
        response_with(
            "200 OK",
            "Set-Cookie: td=abc; Path=/; HttpOnly\r\n",
            b"<script>window.__CSRF_TOKEN__ = 'csrf1';</script>",
        ),
        ok(
            json!({"hasMore": false, "results": [{"deviation": {"deviationId": 123,
            "author": {"username": "alice"}, "isJournal": false}}]}),
        ),
        ok(json!({"deviation": {"extended": {"deviationUuid": UUID}}})),
        token("t"),
        ok(deviation(UUID, 900000123, false)),
    ]);
    let client = with(root, json!({"group": false}));
    let events = run(&client, "https://www.deviantart.com/alice/gallery/scraps").await;
    let file = files(&events)[0];
    assert_eq!(keywords(file)["subcategory"], "scraps");
    assert_eq!(paths(file).1, dir(&["deviantart", "alice", "Scraps"]));
    let requests = server.join().unwrap();
    assert!(line(&requests[0]).starts_with("GET / "));
    assert!(line(&requests[1]).starts_with(
        "GET /_puppy/dashared/gallection/contents?username=alice&type=gallery&scraps_folder=true&offset=0&limit=24&csrf_token=csrf1 "
    ));
    assert!(requests[1].contains("cookie: td=abc") || requests[1].contains("Cookie: td=abc"));
    assert!(line(&requests[2]).starts_with(
        "GET /_puppy/dadeviation/init?deviationid=123&username=alice&type=art&include_session=false&expand=deviation.related&da_minor_version=20230710&csrf_token=csrf1 "
    ));
    assert!(!requests[4].contains("td=abc"));
}

fn journal(index: u64) -> Value {
    json!({"deviationid": UUID, "title": "My Journal", "published_time": 1704067200,
        "url": format!("https://www.deviantart.com/alice/journal/My-Journal-{index}"),
        "excerpt": "Hello", "author": {"username": "Alice"}, "is_downloadable": false})
}

const LITERATURE: &str = "<h2>Literature Text</h2></span><div>Hello &amp; <b>bye</b><br />World</div></section></div></div>";

#[tokio::test]
async fn journals_render_html_and_text_like_upstream() {
    let posts = ok(json!({"results": [journal(456),
        {"statusid": uuid(1), "url": "https://www.deviantart.com/alice/status-update/7"}],
        "has_more": false, "next_cursor": null}));
    let (root, server) = serve(vec![token("t"), posts.clone(), html(LITERATURE)]);
    let client = with(root, json!({"group": false}));
    let events = run(&client, "https://www.deviantart.com/alice/posts/journals").await;
    let media = files(&events);
    assert_eq!(media.len(), 1);
    let file = media[0];
    let MediaJob::InlineText { data } = &file.job else {
        panic!("journal is not inline text");
    };
    assert!(data.starts_with("<!DOCTYPE html>\n<html>\n<head>\n    <meta charset=\"utf-8\">\n    <title>My Journal</title>"));
    assert!(data.contains(r#"<div class="journal journal-green journalcontrol">"#));
    assert!(data.contains(r#"<a class="u regular username" href="https://www.deviantart.com/alice/">Alice</a><span class="user-symbol regular"></span>"#));
    assert!(data.contains("Hello &amp; <b>bye</b><br />World"));
    assert!(data.contains("<span>2024-01-01 00:00:00</span>"));
    let k = keywords(file);
    assert_eq!(
        (&k["extension"], &k["subcategory"]),
        (&json!("htm"), &json!("journal"))
    );
    assert_eq!(
        paths(file),
        (
            "deviantart_456_My Journal.htm".into(),
            dir(&["deviantart", "alice", "Journal"])
        )
    );
    assert_eq!(
        file.archive_key().unwrap(),
        legacy_key(json!([UUID, "journal"]))
    );
    let requests = server.join().unwrap();
    assert!(line(&requests[1]).starts_with(
        "GET /api/v1/oauth2/user/profile/posts?mature_content=true&username=alice&limit=50 "
    ));
    assert!(line(&requests[2]).starts_with("GET /alice/journal/My-Journal-456 "));

    let (root, _server) = serve(vec![token("t"), posts, html(LITERATURE)]);
    let client = with(root, json!({"group": false, "journals": "text"}));
    let events = run(&client, "https://www.deviantart.com/alice/journal").await;
    let MediaJob::InlineText { data } = &files(&events)[0].job else {
        panic!("journal is not inline text");
    };
    assert_eq!(
        data,
        "My Journal\nby Alice, 2024-01-01 00:00:00\n\nHello & bye\nWorld\n"
    );
}

#[tokio::test]
async fn statuses_expand_shared_deviations_and_bodies() {
    let status = json!({"statusid": uuid(1), "is_deleted": false, "body": "Hi <b>all</b>",
        "ts": "2024-01-01T00:00:00-08:00", "comments_count": 2,
        "url": "https://www.deviantart.com/alice/status-update/789",
        "author": {"username": "Alice"},
        "items": [{"deviation": deviation(&uuid(2), 900000002, false)}]});
    let (root, _server) = serve(vec![
        token("t"),
        ok(json!({"results": [status, journal(5)], "has_more": false})),
    ]);
    let client = with(root, json!({"group": false, "journals": "text"}));
    let events = run(&client, "https://www.deviantart.com/alice/posts/statuses").await;
    let media = files(&events);
    assert_eq!(media.len(), 2);
    assert_eq!(keywords(media[0])["deviationid"], uuid(2));
    let file = media[1];
    let k = keywords(file);
    assert_eq!(
        (&k["index"], &k["title"], &k["date"]),
        (
            &json!(789),
            &json!("Status Update"),
            &json!("2024-01-01 08:00:00")
        )
    );
    assert_eq!(k["stats"], json!({"comments": 2}));
    let name = if cfg!(windows) {
        "deviantart_789_Status Update_2024-01-01 08_00_00.txt"
    } else {
        "deviantart_789_Status Update_2024-01-01 08:00:00.txt"
    };
    assert_eq!(
        paths(file),
        (name.into(), dir(&["deviantart", "alice", "Status"]))
    );
    let MediaJob::InlineText { data } = &file.job else {
        panic!("status is not inline text");
    };
    assert_eq!(
        data,
        "Status Update\nby Alice, 2024-01-01 08:00:00\n\nHi all\n"
    );
    assert_eq!(
        file.archive_key().unwrap(),
        legacy_key(json!([uuid(1), "journal"]))
    );
}

#[tokio::test]
async fn tags_with_metadata_and_search_results() {
    let (root, server) = serve(vec![
        token("t"),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
        ok(
            json!({"metadata": [{"deviationid": uuid(1), "tags": [{"tag_name": "cat"}],
            "description": "d"}]}),
        ),
    ]);
    let client = with(root, json!({"metadata": "stats"}));
    let events = run(&client, "https://www.deviantart.com/tag/cute%20cats").await;
    let file = files(&events)[0];
    let k = keywords(file);
    assert_eq!(
        (&k["search_tags"], &k["tags"]),
        (&json!("cute cats"), &json!(["cat"]))
    );
    assert_eq!(k["username"], "Alice");
    assert_eq!(paths(file).1, dir(&["deviantart", "Tags", "cute cats"]));
    let requests = server.join().unwrap();
    assert!(line(&requests[1]).starts_with(
        "GET /api/v1/oauth2/browse/tags?mature_content=true&tag=cute+cats&offset=0&limit=50 "
    ));
    assert!(line(&requests[2]).starts_with(&format!(
        "GET /api/v1/oauth2/deviation/metadata?deviationids%5B0%5D={}&mature_content=true&ext_stats=1 ",
        uuid(1)
    )));

    let results = (1..=4)
        .map(|n| format!(r#"<a href="https://www.deviantart.com/bob/art/Cat-{n}">"#))
        .collect::<String>();
    let (root, server) = serve(vec![
        html(&results),
        html("<script>window.__CSRF_TOKEN__ = 'c';</script>"),
        ok(json!({"deviation": {"extended": {"deviationUuid": UUID}}})),
        token("t"),
        ok(deviation(UUID, 900000001, false)),
    ]);
    let client = with(root, json!({}));
    let events = run(&client, "https://www.deviantart.com/search?q=cats").await;
    let file = files(&events)[0];
    assert_eq!(keywords(file)["search_tags"], "cats");
    assert_eq!(paths(file).1, dir(&["deviantart", "Search", "cats"]));
    let requests = server.join().unwrap();
    assert!(line(&requests[0]).starts_with("GET /search?q=cats "));
    assert!(
        line(&requests[2])
            .starts_with("GET /_puppy/dadeviation/init?deviationid=1&username=bob&type=art")
    );
}

#[tokio::test]
async fn user_profiles_dispatch_included_subcategories() {
    let (root, server) = serve(vec![
        token("t"),
        profile("Alice"),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let client = with(
        root,
        json!({"include": ["avatar", "background", "gallery"]}),
    );
    let events = run(&client, "https://www.deviantart.com/alice").await;
    assert_eq!(children(&events), 3);
    let media = files(&events);
    assert_eq!(media.len(), 3);
    let avatar = media[0];
    assert_eq!(
        avatar.url.as_str(),
        "https://a.deviantart.net/avatars-big/a/l/alice.png?3"
    );
    let k = keywords(avatar);
    assert_eq!(
        (
            &k["subcategory"],
            &k["index"],
            &k["title"],
            &k["da_category"]
        ),
        (
            &json!("avatar"),
            &json!(3),
            &json!("avatar"),
            &json!("avatar")
        )
    );
    assert_eq!(
        paths(avatar),
        (
            "deviantart_3_avatar.png".into(),
            dir(&["deviantart", "Alice"])
        )
    );
    assert_eq!(
        avatar.archive_key().unwrap(),
        legacy_key(json!(["avatar", "alice", "3"]))
    );
    let background = media[1];
    assert_eq!(keywords(background)["subcategory"], "background");
    assert_eq!(
        background.archive_key().unwrap(),
        legacy_key(json!([uuid(7)]))
    );
    assert_eq!(keywords(media[2])["subcategory"], "gallery");
    let requests = server.join().unwrap();
    assert!(line(&requests[2]).starts_with(
        "GET /api/v1/oauth2/gallery/all?mature_content=true&username=Alice&offset=0&limit=24 "
    ));
}

#[tokio::test]
async fn stash_items_resolve_through_their_page() {
    let stash = format!(
        r#"<meta content="DeviantArt://deviation/{UUID}"><script>\"deviationId\":42,</script>"#
    );
    let folder = r#"<a href="https://www.deviantart.com/stash/0abc">x</a><a href="https://www.deviantart.com/stash/2xyz#comments">"#;
    let (root, server) = serve(vec![
        html(folder),
        html(&stash),
        token("t"),
        ok(deviation(UUID, 1, false)),
    ]);
    let client = client(root);
    let events = run(&client, "https://sta.sh/2xyz").await;
    let file = files(&events)[0];
    let k = keywords(file);
    assert_eq!(
        (&k["index"], &k["stash_id"], &k["stash_name"]),
        (&json!(42), &json!("0abc"), &json!(""))
    );
    assert_eq!(k["subcategory"], "stash");
    assert_eq!(file.archive_key().unwrap(), legacy_key(json!([UUID])));
    let requests = server.join().unwrap();
    assert!(line(&requests[0]).starts_with("GET /stash/2xyz "));
    assert!(line(&requests[1]).starts_with("GET /stash/0abc "));
}

#[tokio::test]
async fn refresh_tokens_switch_to_private_access_and_stay_secret() {
    let (root, server) = serve(vec![
        token("pub"),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
        ok(json!({"access_token": "priv", "refresh_token": "rt2"})),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let client = with(root, json!({"group": false})).with_refresh_token(Some("rt1"));
    let events = run(&client, "https://www.deviantart.com/alice/gallery").await;
    let media = files(&events);
    assert_eq!(media.len(), 1);
    assert!(media[0].headers.is_empty());
    let requests = server.join().unwrap();
    assert!(requests[0].contains("grant_type=client_credentials"));
    assert!(requests[2].contains("grant_type=refresh_token&client_id=5388&client_secret="));
    assert!(requests[2].ends_with("&refresh_token=rt1"));
    assert!(
        requests[3]
            .to_ascii_lowercase()
            .contains("authorization: bearer priv")
    );
    assert_eq!(client.refresh_token.lock().unwrap().as_deref(), Some("rt2"));

    let (root, server) = serve(vec![response(
        "400 Bad Request",
        b"{\"error\":\"invalid_grant\"}",
    )]);
    let client = with(root, json!({"group": false, "public": false}))
        .with_refresh_token(Some("secret-token"));
    let error = client
        .stream(&Url::parse("https://www.deviantart.com/alice/gallery").unwrap())
        .collect::<Vec<_>>()
        .await
        .remove(0)
        .unwrap_err()
        .to_string();
    assert!(error.contains("refresh-token"), "{error}");
    assert!(!error.contains("secret-token") && !error.contains("127.0.0.1"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[tokio::test]
async fn cached_refresh_tokens_are_used_and_rotations_persist_across_runs() {
    let directory = tempfile::tempdir().unwrap();
    let cache = crate::cache::Cache::at(directory.path().join("cache.sqlite3"));
    let key = "gallery_dl.extractor.deviantart._refresh_token_cache-#5388";
    cache.set(key, &json!("cached-1"), None).await.unwrap();

    // Without a configured token, the `oauth:deviantart` entry is used and
    // each rotated token replaces it, so a second run starts from the new one.
    for (expected, rotated) in [("cached-1", "cached-2"), ("cached-2", "cached-3")] {
        let (root, server) = serve(vec![
            ok(json!({"access_token": "priv", "refresh_token": rotated})),
            page(vec![deviation(&uuid(1), 900000001, false)], None),
        ]);
        let client = with(root, json!({"group": false, "public": false}))
            .with_refresh_token(Some("cache"))
            .with_token_cache(Some(&cache))
            .await;
        let events = run(&client, "https://www.deviantart.com/alice/gallery").await;
        assert_eq!(files(&events).len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].ends_with(&format!("&refresh_token={expected}")));
        assert_eq!(cache.get_str(key).await.unwrap().as_deref(), Some(rotated));
    }

    // A configured token keys its own entry, holding its latest rotation.
    let (root, server) = serve(vec![
        ok(json!({"access_token": "priv", "refresh_token": "rotated"})),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
        ok(json!({"access_token": "priv", "refresh_token": "rotated-2"})),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    for expected in ["configured", "rotated"] {
        let client = with(root.clone(), json!({"group": false, "public": false}))
            .with_refresh_token(Some("configured"))
            .with_token_cache(Some(&cache))
            .await;
        let events = run(&client, "https://www.deviantart.com/alice/gallery").await;
        assert_eq!(files(&events).len(), 1, "{expected}");
    }
    let requests = server.join().unwrap();
    assert!(requests[0].ends_with("&refresh_token=configured"));
    assert!(requests[2].ends_with("&refresh_token=rotated"));
    assert_eq!(
        cache
            .get_str("gallery_dl.extractor.deviantart._refresh_token_cache-configured")
            .await
            .unwrap()
            .as_deref(),
        Some("rotated-2")
    );
    assert_eq!(
        cache.get_str(key).await.unwrap().as_deref(),
        Some("cached-3")
    );

    // An empty cache leaves public access.
    let (root, server) = serve(vec![
        token("pub"),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let empty = crate::cache::Cache::at(directory.path().join("empty.sqlite3"));
    let client = with(root, json!({"group": false}))
        .with_refresh_token(None)
        .with_token_cache(Some(&empty))
        .await;
    run(&client, "https://www.deviantart.com/alice/gallery").await;
    assert!(server.join().unwrap()[0].contains("grant_type=client_credentials"));
}

#[tokio::test]
async fn rejected_credentials_are_not_echoed() {
    let (root, server) = serve(vec![response("401 Unauthorized", b"{\"error\":\"x\"}")]);
    let error = with(root, json!({"group": false}))
        .with_retries(3)
        .with_credentials(Some("42"), Some("topsecret"))
        .stream(&Url::parse("https://www.deviantart.com/alice/gallery").unwrap())
        .collect::<Vec<_>>()
        .await
        .remove(0)
        .unwrap_err()
        .to_string();
    assert!(error.contains("client-secret"), "{error}");
    assert!(!error.contains("topsecret") && !error.contains("127.0.0.1"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[tokio::test]
async fn following_queues_watched_users_as_profile_children() {
    let friend = |name: &str| {
        json!({"user": {"username": name, "userid": "U"},
        "is_watching": true, "watches_you": false, "lastvisit": null})
    };
    let (root, server) = serve(vec![
        token("t"),
        ok(json!({"results": [friend("Bob"), friend("bad name")],
            "has_more": true, "next_offset": 50})),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
        page(vec![], None),
    ]);
    let client = with(
        root,
        json!({"group": false, "include": "gallery", "flat": true}),
    );
    let events = run(&client, "https://www.deviantart.com/Alice/about#watching").await;
    let scopes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ExtractionEvent::BeginChild(scope) => Some(&scope.metadata),
            _ => None,
        })
        .collect();
    // The watched user is a `following` child dispatching its profile.
    assert_eq!(
        scopes.iter().map(|m| m.subcategory()).collect::<Vec<_>>(),
        ["following", "user"]
    );
    let media = files(&events);
    assert_eq!(media.len(), 1);
    assert_eq!(keywords(media[0])["subcategory"], "gallery");
    assert_eq!(paths(media[0]).1, dir(&["deviantart", "bob"]));
    let requests = server.join().unwrap();
    assert!(line(&requests[1]).starts_with(
        "GET /api/v1/oauth2/user/friends/alice?mature_content=true&offset=0&limit=50 "
    ));
    assert!(line(&requests[2]).starts_with(
        "GET /api/v1/oauth2/gallery/all?mature_content=true&username=bob&offset=0&limit=24 "
    ));
    assert!(line(&requests[3]).starts_with(
        "GET /api/v1/oauth2/user/friends/alice?mature_content=true&offset=50&limit=50 "
    ));
}

#[tokio::test]
async fn watch_feeds_require_and_use_the_private_token() {
    let (root, server) = serve(vec![
        ok(json!({"access_token": "priv", "refresh_token": "rt2"})),
        page(vec![deviation(&uuid(1), 900000001, false)], None),
    ]);
    let client = with(root, json!({})).with_refresh_token(Some("rt1"));
    let events = run(&client, "https://www.deviantart.com/watch/deviations").await;
    let media = files(&events);
    assert_eq!(media.len(), 1);
    let k = keywords(media[0]);
    assert_eq!(
        (&k["subcategory"], &k["username"]),
        (&json!("watch"), &json!("Alice"))
    );
    assert_eq!(
        paths(media[0]),
        (
            "deviantart_900000001_My Art.jpg".into(),
            dir(&["deviantart", "Alice"])
        )
    );
    assert_eq!(
        media[0].archive_key().unwrap(),
        legacy_key(json!([uuid(1)]))
    );
    let requests = server.join().unwrap();
    assert!(requests[0].contains("grant_type=refresh_token"));
    assert!(line(&requests[1]).starts_with(
        "GET /api/v1/oauth2/browse/deviantsyouwatch?mature_content=true&offset=0&limit=50 "
    ));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer priv")
    );

    // Posts unpack each result's journal; entries without one are skipped.
    let (root, server) = serve(vec![
        ok(json!({"access_token": "priv"})),
        ok(
            json!({"results": [{"journal": journal(456)}, {"status": {}}],
            "has_more": false, "next_offset": null}),
        ),
        html(LITERATURE),
    ]);
    let client = with(root, json!({})).with_refresh_token(Some("rt1"));
    let events = run(&client, "https://www.deviantart.com/watch/posts").await;
    let media = files(&events);
    assert_eq!(media.len(), 1);
    assert_eq!(keywords(media[0])["subcategory"], "watch-posts");
    assert_eq!(
        paths(media[0]),
        (
            "deviantart_456_My Journal.htm".into(),
            dir(&["deviantart", "Alice"])
        )
    );
    assert_eq!(
        media[0].archive_key().unwrap(),
        legacy_key(json!([UUID, "journal"]))
    );
    let requests = server.join().unwrap();
    assert!(line(&requests[1]).starts_with(
        "GET /api/v1/oauth2/browse/posts/deviantsyouwatch?mature_content=true&offset=0&limit=50 "
    ));

    // Without a refresh token nothing is requested.
    let (root, server) = serve(vec![]);
    let error = with(root, json!({}))
        .stream(&Url::parse("https://www.deviantart.com/watch/posts").unwrap())
        .collect::<Vec<_>>()
        .await
        .remove(0)
        .unwrap_err()
        .to_string();
    assert!(error.contains("refresh-token"), "{error}");
    assert!(server.join().unwrap().is_empty());
}
