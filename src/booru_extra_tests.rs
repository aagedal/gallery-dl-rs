//! Upstream Danbooru `random`/`media-asset`, e621 short links/frontends,
//! Gelbooru redirects and the `metadata`/`tags`/`notes` options.
use super::{
    tests::{booru, danbooru_post, e621_post, response, serve},
    *,
};
use serde_json::json;

const MD5: &str = "0123456789abcdef0123456789abcdef";

fn t(site: Site, raw: &str) -> Target {
    target(site, &Url::parse(raw).unwrap()).unwrap().0
}

fn media(events: &[Result<ExtractionEvent>]) -> Vec<&Media> {
    events
        .iter()
        .filter_map(|event| match event.as_ref().unwrap() {
            ExtractionEvent::Media(media) => Some(media.as_ref()),
            _ => None,
        })
        .collect()
}

fn keywords(media: &Media) -> &serde_json::Map<String, Value> {
    media
        .metadata
        .as_ref()
        .unwrap()
        .booru_keywords
        .as_ref()
        .unwrap()
}

fn paths(media: &Media) -> (String, String) {
    let file = crate::naming::effective_filename(media, None)
        .unwrap()
        .render(media)
        .unwrap();
    let directory = crate::naming::effective_directory(media, None)
        .unwrap()
        .render(media)
        .unwrap()
        .unwrap();
    (file, directory.to_string_lossy().replace('\\', "/"))
}

#[test]
fn classifies_upstream_classes() {
    assert_eq!(
        t(
            Site::Danbooru,
            "https://danbooru.donmai.us/posts/random?x=1&tags=a+b%21&tags=c+d"
        ),
        Target::Random { tags: "c d".into() }
    );
    assert_eq!(
        t(Site::Danbooru, "https://danbooru.donmai.us/posts/random"),
        Target::Random { tags: "".into() }
    );
    assert_eq!(
        t(Site::Danbooru, "https://danbooru.donmai.us/media_assets/77"),
        Target::MediaAsset { id: 77 }
    );
    // `/p/(\w+)` is a base-32 post ID: 1 * 32 + 31.
    assert_eq!(
        t(Site::E621, "https://e621.net/p/1v"),
        Target::Post { id: 63 }
    );
    assert_eq!(
        t(Site::E621, "https://e621.cc/posts/5"),
        Target::Post { id: 5 }
    );
    let (_, root) = target(Site::E621, &Url::parse("https://e621.cc/posts/5").unwrap()).unwrap();
    assert_eq!(root.as_str(), "https://e621.net/");
    assert_eq!(
        t(Site::E621, "https://e621.cc/?tags=cat+dog&x=1"),
        Target::Frontend {
            tags: "cat+dog".into()
        }
    );
    assert_eq!(
        t(Site::E621, "https://e621.anthro.fr/?q=fox"),
        Target::Frontend { tags: "fox".into() }
    );
    assert_eq!(
        t(Site::Gelbooru, "https://gelbooru.com/redirect.php?s=YWJj&x"),
        Target::Redirect {
            encoded: "YWJj".into()
        }
    );
    for (site, raw) in [
        (Site::Danbooru, "https://danbooru.donmai.us/media_assets/x"),
        (Site::E621, "https://e621.net/p/!"),
        (Site::E621, "https://e621.anthro.fr/posts/1"),
        (Site::Gelbooru, "https://gelbooru.com/redirect.php"),
        (Site::Safebooru, "https://safebooru.org/redirect.php?s=YWJj"),
    ] {
        assert!(target(site, &Url::parse(raw).unwrap()).is_err(), "{raw}");
    }
    for (raw, subcategory) in [
        ("https://danbooru.donmai.us/posts/random?tags=a", "random"),
        ("https://danbooru.donmai.us/media_assets/1", "media-asset"),
        ("https://e621.cc/?tags=a", "frontend"),
        ("https://gelbooru.com/redirect.php?s=YWJj", "redirect"),
    ] {
        let defaults = defaults_for_url(&Url::parse(raw).unwrap()).unwrap();
        assert_eq!(defaults.subcategory, subcategory, "{raw}");
    }
}

#[tokio::test]
async fn random_posts_use_the_search_and_tag_defaults() {
    let body = danbooru_post(9).to_string();
    let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
    let events: Vec<_> = booru(Site::Danbooru, root)
        .events(&Url::parse("https://danbooru.donmai.us/posts/random?tags=a+b").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    let metadata = files[0].metadata.as_ref().unwrap();
    assert_eq!(metadata.search_tags.as_deref(), Some("a b"));
    assert_eq!(
        metadata.source_defaults.as_ref().unwrap().archive,
        "t_{search_tags}_{id}"
    );
    assert_eq!(
        paths(files[0]),
        (format!("danbooru_9_{MD5}.png"), "danbooru/a b".into())
    );
    let requests = server.join().unwrap();
    assert!(
        requests[0].starts_with("GET /posts/random.json?tags=a+b "),
        "{}",
        requests[0]
    );

    let (root, server) = serve(vec![response(
        "200 OK",
        "",
        json!([danbooru_post(3)]).to_string().as_bytes(),
    )]);
    let events: Vec<_> = booru(Site::Danbooru, root)
        .events(&Url::parse("https://danbooru.donmai.us/posts/random").unwrap())
        .collect()
        .await;
    assert_eq!(media(&events).len(), 1);
    assert!(server.join().unwrap()[0].starts_with("GET /posts/random.json "));
}

#[tokio::test]
async fn media_assets_use_the_last_variant_and_their_own_archive_space() {
    let asset = json!({"id": 9, "md5": MD5, "file_ext": "png", "file_size": 7,
    "image_width": 4, "image_height": 3, "created_at": "2024-01-02T03:04:05Z",
    "variants": [
        {"type": "180x180", "url": format!("https://cdn.donmai.us/180x180/01/23/{MD5}.jpg")},
        {"type": "original", "url": format!("https://cdn.donmai.us/original/01/23/{MD5}.png")}
    ]});
    let (root, server) = serve(vec![response("200 OK", "", asset.to_string().as_bytes())]);
    let events: Vec<_> = booru(Site::Danbooru, root)
        .events(&Url::parse("https://danbooru.donmai.us/media_assets/9").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    assert_eq!(
        files[0].url.as_str(),
        format!("https://cdn.donmai.us/original/01/23/{MD5}.png")
    );
    assert_eq!(keywords(files[0])["tag_string_artist"], "");
    assert_eq!(
        paths(files[0]),
        (format!("danbooru_ma9_{MD5}.png"), "danbooru".into())
    );
    let mut post = parse_post(Site::Danbooru, &danbooru_post(9)).unwrap();
    post.metadata.as_mut().unwrap().source_defaults =
        defaults_for_url(&Url::parse("https://danbooru.donmai.us/posts/9").unwrap());
    assert!(files[0].archive_key().is_some());
    assert_ne!(files[0].archive_key(), post.archive_key());
    assert!(server.join().unwrap()[0].starts_with("GET /media_assets/9.json "));
}

#[tokio::test]
async fn frontends_and_redirects_queue_lazy_child_jobs() {
    let page = json!({"posts": [e621_post(4)]}).to_string();
    let (root, server) = serve(vec![response("200 OK", "", page.as_bytes())]);
    let client = booru(Site::E621, root);
    let events: Vec<_> = client
        .events(&Url::parse("https://e621.cc/?tags=cat+dog").unwrap())
        .collect()
        .await;
    assert!(matches!(
        events[0].as_ref().unwrap(),
        ExtractionEvent::BeginChild(_)
    ));
    assert_eq!(media(&events).len(), 1);
    let requests = server.join().unwrap();
    assert!(
        requests[0].starts_with("GET /posts.json?tags=cat+dog&limit=320 "),
        "{}",
        requests[0]
    );

    // base64("gelbooru.com/index.php?page=post&s=view&id=9"), padding encoded.
    let encoded = "Z2VsYm9vcnUuY29tL2luZGV4LnBocD9wYWdlPXBvc3Qmcz12aWV3JmlkPTk%3D";
    let post = json!({"post": [{"id": 9, "md5": MD5, "tags": "a",
        "created_at": "Sat Jan 02 03:04:05 -0500 2024",
        "file_url": format!("https://img3.gelbooru.com/images/01/23/{MD5}.jpg")}]});
    let (root, server) = serve(vec![response("200 OK", "", post.to_string().as_bytes())]);
    let events: Vec<_> = booru(Site::Gelbooru, root)
        .events(&Url::parse(&format!("https://gelbooru.com/redirect.php?s={encoded}")).unwrap())
        .collect()
        .await;
    assert_eq!(media(&events).len(), 1, "{events:?}");
    let requests = server.join().unwrap();
    assert!(requests[0].contains("id=9"), "{}", requests[0]);
    let invalid: Vec<_> = booru(Site::Gelbooru, Url::parse("http://127.0.0.1:9/").unwrap())
        .events(&Url::parse("https://gelbooru.com/redirect.php?s=%FF%FE").unwrap())
        .collect()
        .await;
    assert!(invalid.iter().any(Result::is_err));
}

#[test]
fn danbooru_ugoira_zips_download_their_webm_sample() {
    let mut post = danbooru_post(5);
    post["file_url"] = json!(format!("https://cdn.donmai.us/original/01/23/{MD5}.zip"));
    post["file_ext"] = json!("zip");
    post["large_file_url"] = json!(format!("https://cdn.donmai.us/sample/01/23/{MD5}.webm"));
    let media = parse_post(Site::Danbooru, &post).unwrap();
    assert_eq!(
        media.url.as_str(),
        format!("https://cdn.donmai.us/sample/01/23/{MD5}.webm")
    );
    assert_eq!(keywords(&media)["extension"], "webm");
    assert_eq!(keywords(&media)["filename"], MD5);
    assert!(media.filename.ends_with(".webm"));
    post.as_object_mut().unwrap().remove("large_file_url");
    assert!(parse_post(Site::Danbooru, &post).is_none());
}

#[test]
fn option_values_follow_upstream_truthiness() {
    let parse =
        |site, value: Value| options::Enrichment::parse(site, value.as_object().unwrap()).unwrap();
    assert_eq!(
        parse(Site::Danbooru, json!({"metadata": true}))
            .includes
            .as_deref(),
        Some("artist_commentary,children,notes,parent,uploader,id")
    );
    assert_eq!(
        parse(Site::Danbooru, json!({"metadata": ["notes", "pools"]}))
            .includes
            .as_deref(),
        Some("notes,pools,id")
    );
    assert_eq!(
        parse(Site::Danbooru, json!({"metadata": "notes"}))
            .includes
            .as_deref(),
        Some("notes,id")
    );
    assert!(!parse(Site::Danbooru, json!({"metadata": ""})).any());
    let e621 = parse(Site::E621, json!({"metadata": 1}));
    assert!(e621.notes && e621.pools);
    let e621 = parse(Site::E621, json!({"metadata": "pools,x"}));
    assert!(!e621.notes && e621.pools);
    let gel = parse(Site::Gelbooru, json!({"tags": true, "notes": 0}));
    assert!(gel.html_tags && !gel.html_notes);
    assert!(!parse(Site::Safebooru, json!({"metadata": true})).any());
    assert!(
        options::Enrichment::parse(
            Site::Danbooru,
            json!({"metadata": [1]}).as_object().unwrap()
        )
        .is_err()
    );
}

#[tokio::test]
async fn danbooru_metadata_merges_one_includes_request_per_page() {
    let page = json!([danbooru_post(1), danbooru_post(2)]).to_string();
    let extra = json!([{"id": 2, "notes": [{"body": "two"}]},
        {"id": 1, "artist_commentary": {"original_title": "one"}}])
    .to_string();
    let single = json!({"id": 7, "uploader": {"name": "alice"}}).to_string();
    let (root, server) = serve(vec![
        response("200 OK", "", page.as_bytes()),
        response("200 OK", "", extra.as_bytes()),
        response("200 OK", "", danbooru_post(7).to_string().as_bytes()),
        response("200 OK", "", single.as_bytes()),
    ]);
    let options = json!({"metadata": ["notes", "artist_commentary"]});
    let client = booru(Site::Danbooru, root)
        .with_options(options.as_object())
        .unwrap();
    let events: Vec<_> = client
        .events(&Url::parse("https://danbooru.donmai.us/posts?tags=cat").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 2, "{events:?}");
    assert_eq!(
        keywords(files[0])["artist_commentary"]["original_title"],
        "one"
    );
    assert_eq!(keywords(files[1])["notes"][0]["body"], "two");
    let events: Vec<_> = client
        .events(&Url::parse("https://danbooru.donmai.us/posts/7").unwrap())
        .collect()
        .await;
    assert_eq!(keywords(media(&events)[0])["uploader"]["name"], "alice");
    let requests = server.join().unwrap();
    assert!(
        requests[1].starts_with(
            "GET /posts.json?only=notes%2Cartist_commentary%2Cid&limit=2&tags=id%3A1%2C2 "
        ),
        "{}",
        requests[1]
    );
    assert!(requests[3].starts_with("GET /posts/7.json?only=notes%2Cartist_commentary%2Cid "));
}

#[tokio::test]
async fn e621_metadata_adds_notes_and_cached_pools() {
    let mut first = e621_post(1);
    first["has_notes"] = json!(true);
    first["pools"] = json!([3]);
    let mut second = e621_post(2);
    second["has_notes"] = json!(false);
    second["pools"] = json!([3]);
    let page = json!({"posts": [first, second]}).to_string();
    let (root, server) = serve(vec![
        response("200 OK", "", page.as_bytes()),
        response("200 OK", "", br#"[{"body":"note"}]"#),
        response("200 OK", "", br#"[{"id":3,"name":"my_pool"}]"#),
    ]);
    let options = json!({"metadata": true});
    let events: Vec<_> = booru(Site::E621, root)
        .with_options(options.as_object())
        .unwrap()
        .events(&Url::parse("https://e621.net/posts?tags=fox").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 2, "{events:?}");
    assert_eq!(keywords(files[0])["notes"], json!([{"body": "note"}]));
    assert!(keywords(files[1]).get("notes").is_none());
    for file in &files {
        assert_eq!(
            keywords(file)["pools"],
            json!([{"id": 3, "name": "my pool"}])
        );
    }
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].starts_with("GET /notes.json?search%5Bpost_id%5D=1 "));
    assert!(requests[2].starts_with("GET /pools.json?search%5Bid%5D=3 "));
}

const GELBOORU_PAGE: &str = r#"<ul id="tag-list">
    <li class="tag-type-artist"><a href="index.php?page=post&amp;s=list&amp;tags=some_artist">?</a>
    <a href="index.php?page=post&s=list&tags=some_artist">some artist</a></li>
    <li class="tag-type-general"><a href="index.php?page=post&s=list&amp;tags=cat%27s">cat's</a></li>
    <li class="tag-type-general"><a href="index.php?page=post&s=list&amp;tags=a%26amp%3Bb">a&b</a></li>
    </ul>
    <section id="notes"><article data-width="10" data-height="20" data-x="1" data-y="2"
    data-body="hello &amp; bye"></article><article data-width="x"></article></section>"#;

#[test]
fn post_pages_provide_category_tags_and_notes() {
    let tags = options::html_tags(GELBOORU_PAGE);
    assert_eq!(tags["tags_artist"], "some_artist");
    assert_eq!(tags["tags_general"], "cat's a&b");
    assert_eq!(
        options::gelbooru_notes(GELBOORU_PAGE).unwrap(),
        [json!({"width": 10, "height": 20, "x": 1, "y": 2, "body": "hello &amp; bye"})]
    );
    let safebooru = r#"<div id="note-container">
        <div class="note-box" style="width: 30px; height: 40px; top: 5px; left: 6px;" id="note-box-11">
        <div class="note-body" id="note-body-11" title="x">Hi <b>there</b> &amp; you</div>
        </div><img src="x">"#;
    assert_eq!(
        options::v02_notes(safebooru).unwrap(),
        [json!({"width": 30, "height": 40, "y": 5, "x": 6, "id": 11, "body": "Hi there & you"})]
    );
    assert!(options::v02_notes("<html>").is_none());
    assert!(options::html_tags("<html>").is_empty());
}

#[tokio::test]
async fn gelbooru_tags_and_notes_fetch_each_post_page() {
    let post = |id: u64| {
        json!({"id": id, "md5": MD5, "tags": "a", "created_at": "Sat Jan 02 03:04:05 -0500 2024",
            "file_url": format!("https://img3.gelbooru.com/images/01/23/{MD5}.jpg")})
    };
    let page = json!({"post": [post(1), {"id": 2, "file_url": ""}]}).to_string();
    let (root, server) = serve(vec![
        response("200 OK", "", page.as_bytes()),
        response("200 OK", "", GELBOORU_PAGE.as_bytes()),
    ]);
    let options = json!({"tags": true, "notes": true});
    let events: Vec<_> = booru(Site::Gelbooru, root)
        .with_options(options.as_object())
        .unwrap()
        .events(&Url::parse("https://gelbooru.com/index.php?page=post&s=list&tags=a").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    assert_eq!(keywords(files[0])["tags_artist"], "some_artist");
    assert_eq!(keywords(files[0])["notes"][0]["width"], 10);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].starts_with("GET /index.php?page=post&s=view&id=1 "));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("accept: text/html")
    );
}
