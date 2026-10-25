//! Upstream's additional Danbooru (`atfbooru`, `aibooru`, `booruvar`) and
//! e621 (`e6ai`) instances: routing, roots, CDN hosts, output identity and
//! instance-scoped credentials.
use super::{
    tests::{booru, response, serve},
    *,
};
use serde_json::json;

const MD5: &str = "0123456789abcdef0123456789abcdef";

fn t(site: Site, raw: &str) -> Result<(Target, Url)> {
    target(site, &Url::parse(raw).unwrap())
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

fn danbooru_dialect_post(id: u64, file_url: &str) -> Value {
    json!({"id": id, "created_at": "2024-01-02T03:04:05.123-05:00", "md5": MD5,
           "file_ext": "png", "image_width": 8, "image_height": 6, "file_size": 12,
           "tag_string": "1girl solo", "tag_string_artist": "", "uploader_name": "alice",
           "file_url": file_url})
}

fn e6ai_post(id: u64, file_url: Value) -> Value {
    json!({"id": id, "created_at": "2024-01-02T08:04:05.000Z",
           "file": {"url": file_url, "ext": "png", "md5": MD5, "width": 8, "height": 6, "size": 9},
           "tags": {"general": ["b", "a"], "artist": ["c"]}})
}

/// (instance, input host, API root, dialect)
const INSTANCES: &[(Site, &str, &str, Dialect)] = &[
    (
        Site::Atfbooru,
        "booru.allthefallen.moe",
        "https://booru.allthefallen.moe/",
        Dialect::Danbooru,
    ),
    (
        Site::Aibooru,
        "aibooru.online",
        "https://aibooru.online/",
        Dialect::Danbooru,
    ),
    (
        Site::Aibooru,
        "aibooru.download",
        "https://aibooru.download/",
        Dialect::Danbooru,
    ),
    (
        Site::Aibooru,
        "safe.aibooru.online",
        "https://safe.aibooru.online/",
        Dialect::Danbooru,
    ),
    (
        Site::Aibooru,
        "general.aibooru.download",
        "https://general.aibooru.download/",
        Dialect::Danbooru,
    ),
    (
        Site::Booruvar,
        "booru.borvar.art",
        "https://booru.borvar.art/",
        Dialect::Danbooru,
    ),
    (Site::E6ai, "e6ai.net", "https://e6ai.net/", Dialect::E621),
];

#[test]
fn instances_have_their_own_category_and_cache_slot_but_share_a_dialect() {
    assert_eq!(Site::COUNT, Site::ALL.len());
    for (index, site) in Site::ALL.into_iter().enumerate() {
        assert_eq!(site.slot(), Some(index));
        assert_eq!(Site::from_category(site.category()), Some(site));
    }
    for (site, category, dialect) in [
        (Site::Atfbooru, "atfbooru", Dialect::Danbooru),
        (Site::Aibooru, "aibooru", Dialect::Danbooru),
        (Site::Booruvar, "booruvar", Dialect::Danbooru),
        (Site::E6ai, "e6ai", Dialect::E621),
        (Site::Danbooru, "danbooru", Dialect::Danbooru),
        (Site::E621, "e621", Dialect::E621),
    ] {
        assert_eq!(site.category(), category);
        assert_eq!(site.dialect(), dialect);
        assert!(is_danbooru_family(category));
    }
    assert_eq!(category_dialect("e926"), Some(Dialect::E621));
    assert_eq!(Dialect::Danbooru.base_category(), Some("Danbooru"));
    assert_eq!(Dialect::E621.base_category(), Some("E621"));
    assert_eq!(Dialect::Gelbooru.base_category(), None);
    assert!(!is_danbooru_family("gelbooru") && !is_danbooru_family("Danbooru"));
    // The original sites keep their own upstream hosts.
    for host in ["danbooru.donmai.us", "e621.net", "e926.net", "gelbooru.com"] {
        assert_eq!(Site::from_instance_host(host), None, "{host}");
    }
}

#[test]
fn routes_exact_upstream_hosts_to_their_instance_and_root() {
    for (site, host, root, dialect) in INSTANCES {
        assert_eq!(Site::from_instance_host(host), Some(*site), "{host}");
        let url = Url::parse(&format!("https://{host}/posts/22")).unwrap();
        assert_eq!(
            crate::extract::social_site(&url),
            Some(site.category()),
            "{host}"
        );
        assert_eq!(
            t(*site, url.as_str()).unwrap(),
            (Target::Post { id: 22 }, Url::parse(root).unwrap())
        );
        // Every dialect route is available on every instance of the family.
        let routes: &[(&str, Target)] = if *dialect == Dialect::Danbooru {
            &[
                ("/pools/9", Target::Pool { id: "9".into() }),
                ("/favorite_groups/7", Target::Favgroup { id: "7".into() }),
                ("/posts?tags=a+b", Target::Tags { tags: "a b".into() }),
                ("/posts/random?tags=c", Target::Random { tags: "c".into() }),
                ("/media_assets/5", Target::MediaAsset { id: 5 }),
                ("/explore/posts/popular", Target::Popular { params: vec![] }),
                ("/popular", Target::Popular { params: vec![] }),
                ("/artists/3", Target::Artist { id: "3".into() }),
            ]
        } else {
            &[
                ("/pools/3", Target::Pool { id: "3".into() }),
                (
                    "/posts?tags=anry",
                    Target::Tags {
                        tags: "anry".into(),
                    },
                ),
                ("/p/a", Target::Post { id: 10 }),
                ("/favorites", Target::Favorites { params: vec![] }),
                ("/explore/posts/popular", Target::Popular { params: vec![] }),
                ("/artists/3", Target::Artist { id: "3".into() }),
            ]
        };
        for (path, expected) in routes {
            let url = format!("https://{host}{path}");
            assert_eq!(&t(*site, &url).unwrap().0, expected, "{url}");
        }
    }
    // Lookalike, aliased, foreign-instance and credential-bearing URLs fail.
    for (site, raw) in [
        (Site::Atfbooru, "https://allthefallen.moe/posts/1"),
        (Site::Atfbooru, "https://www.booru.allthefallen.moe/posts/1"),
        (Site::Aibooru, "https://aibooru.online.example/posts/1"),
        (Site::Aibooru, "https://evil.aibooru.online/posts/1"),
        (Site::Aibooru, "https://booru.borvar.art/posts/1"),
        (Site::Booruvar, "https://borvar.art/posts/1"),
        (Site::E6ai, "https://e621.net/posts/1"),
        (Site::E6ai, "https://e6ai.net:444/posts/1"),
        (Site::E6ai, "https://user:pass@e6ai.net/posts/1"),
        (Site::Danbooru, "https://aibooru.online/posts/1"),
        (Site::E621, "https://e6ai.net/posts/1"),
        // Favorite groups are Danbooru-dialect, frontends e621.net-only.
        (Site::E6ai, "https://e6ai.net/favorite_groups/1"),
        (Site::E6ai, "https://e6ai.net/?tags=cat"),
    ] {
        assert!(t(site, raw).is_err(), "{raw}");
    }
    for raw in [
        "https://evil.aibooru.online/posts/1",
        "https://allthefallen.moe/posts/1",
    ] {
        assert_eq!(crate::extract::social_site(&Url::parse(raw).unwrap()), None);
    }
}

#[test]
fn source_defaults_and_archive_descriptors_use_the_instance_category() {
    for (raw, category, format, subcategory) in [
        ("https://aibooru.online/posts/1", "aibooru", "{id}", "post"),
        (
            "https://safe.aibooru.online/posts?tags=center_frills",
            "aibooru",
            "t_{search_tags}_{id}",
            "tag",
        ),
        (
            "https://booru.allthefallen.moe/pools/9",
            "atfbooru",
            "p_{pool[id]}_{id}",
            "pool",
        ),
        (
            "https://booru.borvar.art/favorite_groups/2",
            "booruvar",
            "fg_{favgroup[id]}_{id}",
            "favgroup",
        ),
        (
            "https://booru.borvar.art/media_assets/2",
            "booruvar",
            "m{id}",
            "media-asset",
        ),
        (
            "https://e6ai.net/favorites?user_id=5",
            "e6ai",
            "f_{user_id}_{id}",
            "favorite",
        ),
        (
            "https://e6ai.net/explore/posts/popular",
            "e6ai",
            "P_{scale[0]}_{date}_{id}",
            "popular",
        ),
    ] {
        let url = Url::parse(raw).unwrap();
        let defaults = defaults_for_url(&url).unwrap();
        assert_eq!(defaults.category, category, "{raw}");
        assert_eq!(defaults.archive, format, "{raw}");
        assert_eq!(defaults.subcategory, subcategory, "{raw}");
        let descriptor = crate::source_archive::for_url(&url).unwrap();
        assert_eq!(descriptor.category, category, "{raw}");
        assert_eq!(descriptor.format, format, "{raw}");
        assert!(crate::source_archive::default_keywords_supported(
            &descriptor
        ));
    }
    // Dialect-specific archive formats stay with their dialect.
    for (category, format, supported) in [
        ("aibooru", "fg_{favgroup[id]}_{id}", true),
        ("e6ai", "fg_{favgroup[id]}_{id}", false),
        ("e6ai", "f_{user_id}_{id}", true),
        ("atfbooru", "f_{user_id}_{id}", false),
        ("booruvar", "m{id}", true),
        ("e6ai", "m{id}", false),
        ("e6ai", "p_{pool}_{id}", false),
    ] {
        let descriptor = crate::source_archive::ArchiveDescriptor {
            category: category.into(),
            format: format.into(),
        };
        assert_eq!(
            crate::source_archive::default_keywords_supported(&descriptor),
            supported,
            "{category} {format}"
        );
    }
}

#[test]
fn media_must_come_from_the_instance_cdn() {
    let aibooru = |url: &str| parse_post(Site::Aibooru, &danbooru_dialect_post(1, url));
    let cdn = format!("https://cdn.aibooru.download/original/01/23/{MD5}.png");
    let media = aibooru(&cdn).unwrap();
    assert_eq!(media.category, "aibooru");
    assert_eq!(media.url.as_str(), cdn);
    assert_eq!(media.filename, format!("aibooru_1_{MD5}.png"));
    for foreign in [
        format!("https://cdn.donmai.us/original/01/23/{MD5}.png"),
        format!("https://aibooru.download.example/{MD5}.png"),
        format!("http://cdn.aibooru.download/original/01/23/{MD5}.png"),
        format!("/data/original/01/23/{MD5}.png"),
    ] {
        assert!(aibooru(&foreign).is_none(), "{foreign}");
    }
    // Danbooru itself refuses the other instances' CDNs and vice versa.
    assert!(parse_post(Site::Danbooru, &danbooru_dialect_post(1, &cdn)).is_none());
    // Fixed-root instances resolve root-relative files like upstream.
    for (site, raw, expected) in [
        (
            Site::Booruvar,
            format!("/data/original/01/23/{MD5}.png"),
            format!("https://booru.borvar.art/data/original/01/23/{MD5}.png"),
        ),
        (
            Site::Atfbooru,
            format!("//booru.allthefallen.moe/data/{MD5}.png"),
            format!("https://booru.allthefallen.moe/data/{MD5}.png"),
        ),
    ] {
        let media = parse_post(site, &danbooru_dialect_post(2, &raw)).unwrap();
        assert_eq!(media.url.as_str(), expected);
        assert_eq!(media.category, site.category());
    }
    assert!(
        parse_post(
            Site::Atfbooru,
            &danbooru_dialect_post(2, "//cdn.donmai.us/data/x.png")
        )
        .is_none()
    );
    // e621-dialect hidden files are rebuilt on the instance's static host.
    let hidden = parse_post(Site::E6ai, &e6ai_post(3, Value::Null)).unwrap();
    assert_eq!(
        hidden.url.as_str(),
        format!("https://static1.e6ai.net/data/01/23/{MD5}.png")
    );
    assert_eq!(hidden.category, "e6ai");
    assert!(
        parse_post(
            Site::E6ai,
            &e6ai_post(
                3,
                json!(format!("https://static1.e621.net/data/01/23/{MD5}.png"))
            )
        )
        .is_none()
    );
    assert!(
        parse_post(
            Site::E621,
            &e6ai_post(
                3,
                json!(format!("https://static1.e6ai.net/data/01/23/{MD5}.png"))
            )
        )
        .is_none()
    );
}

#[tokio::test]
async fn danbooru_instances_extract_with_their_category_paths_and_credentials() {
    let cdn = format!("https://cdn.aibooru.download/original/01/23/{MD5}.png");
    let page = json!([danbooru_dialect_post(7, &cdn)]).to_string();
    let (root, server) = serve(vec![response("200 OK", "", page.as_bytes())]);
    let client = booru(Site::Aibooru, root)
        .with_credentials(Some("ai-user"), Some("ai-key"))
        .unwrap();
    let events: Vec<_> = client
        .events(&Url::parse("https://general.aibooru.online/posts?tags=center_frills").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    let file = files[0];
    assert_eq!(file.category, "aibooru");
    assert_eq!(file.url.as_str(), cdn);
    assert_eq!(
        paths(file),
        (
            format!("aibooru_7_{MD5}.png"),
            "aibooru/center_frills".into()
        )
    );
    let key = file.archive_key().unwrap();
    assert!(key.starts_with("aibooru:"), "{key}");
    let descriptor = crate::source_archive::for_media(file).unwrap();
    assert_eq!(
        (descriptor.category.as_str(), descriptor.format.as_str()),
        ("aibooru", "t_{search_tags}_{id}")
    );
    let context = crate::postprocess::context(file);
    assert_eq!(context["category"], "aibooru");
    assert_eq!(context["search_tags"], "center_frills");
    let raw = file
        .metadata
        .as_ref()
        .unwrap()
        .booru_keywords
        .as_ref()
        .unwrap();
    assert_eq!(raw["tags_artist"], json!([]));
    // Credentials reach only the instance API request, never the media.
    let serialized = serde_json::to_string(file).unwrap();
    assert!(!serialized.contains("ai-key") && !serialized.contains("ai-user"));
    assert!(file.headers.is_empty());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("GET /posts.json?tags=center_frills&limit=200"),
        "{}",
        requests[0]
    );
    use base64::Engine;
    let basic = base64::engine::general_purpose::STANDARD.encode("ai-user:ai-key");
    assert!(
        requests[0].to_ascii_lowercase().contains(&format!(
            "authorization: basic {}",
            basic.to_ascii_lowercase()
        )),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn e6ai_extracts_with_e621_dialect_and_its_own_identity() {
    let body = json!({"post": e6ai_post(5, Value::Null)}).to_string();
    let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
    let events: Vec<_> = booru(Site::E6ai, root)
        .events(&Url::parse("https://e6ai.net/posts/5").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    let file = files[0];
    assert_eq!(file.category, "e6ai");
    assert_eq!(
        file.url.as_str(),
        format!("https://static1.e6ai.net/data/01/23/{MD5}.png")
    );
    assert_eq!(paths(file), (format!("e6ai_5_{MD5}.png"), "e6ai".into()));
    assert_eq!(crate::postprocess::context(file)["category"], "e6ai");
    let raw = file
        .metadata
        .as_ref()
        .unwrap()
        .booru_keywords
        .as_ref()
        .unwrap();
    assert_eq!(raw["tags"], json!(["a", "b", "c"]));
    assert_eq!(raw["tags_artist"], json!(["c"]));
    // The same post ID on e621 is a different archive entry.
    let e621 = parse_post(
        Site::E621,
        &e6ai_post(
            5,
            json!(format!("https://static1.e621.net/data/01/23/{MD5}.png")),
        ),
    )
    .unwrap();
    assert_ne!(file.archive_key(), e621.archive_key());
    assert!(file.archive_key().unwrap().starts_with("e6ai:"));
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /posts/5.json "));
    assert!(!requests[0].to_ascii_lowercase().contains("authorization"));
}

#[test]
fn compatibility_output_of_the_instances_is_dedicated_booru_output() {
    let cdn = format!("https://cdn.aibooru.download/original/01/23/{MD5}.png");
    for (category, data) in [
        (
            "aibooru",
            json!({"id": 42, "md5": MD5, "filename": MD5, "extension": "png",
                   "pool": {"id": 9}, "favgroup": {"id": 3}, "search_tags": "x",
                   "rating": "g", "score": 4}),
        ),
        (
            "e6ai",
            json!({"id": 42, "file": {"md5": MD5}, "md5": MD5, "filename": MD5,
                   "extension": "png", "pool": {"id": 9}, "favgroup": {"id": 3},
                   "user_id": 5, "score": {"total": 4}}),
        ),
    ] {
        let message = serde_json::to_vec(&json!([[3, cdn, data]])).unwrap();
        let media = crate::extract::parse_messages(&message, category)
            .unwrap()
            .remove(0);
        assert!(!crate::extract::generic_category(category));
        assert_eq!(media.category, category);
        let metadata = media.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "42");
        assert_eq!(metadata.media_id, MD5);
        assert!(metadata.pool.is_some());
        assert_eq!(metadata.favgroup.is_some(), category == "aibooru");
        assert_eq!(metadata.booru_user_id.is_some(), category == "e6ai");
        assert_eq!(metadata.score, Some(4));
        let native = parse_post(
            Site::from_category(category).unwrap(),
            &if category == "aibooru" {
                danbooru_dialect_post(42, &cdn)
            } else {
                e6ai_post(42, Value::Null)
            },
        )
        .unwrap();
        assert_eq!(media.archive_key(), native.archive_key(), "{category}");
    }
}

#[test]
fn danbooru_accepts_exactly_upstreams_instance_hosts_with_their_own_root() {
    for host in [
        "danbooru.donmai.us",
        "hijiribe.donmai.us",
        "sonohara.donmai.us",
        "safebooru.donmai.us",
        "donmai.moe",
    ] {
        assert!(danbooru_host(host), "{host}");
        let post = Url::parse(&format!("https://{host}/posts/22")).unwrap();
        assert_eq!(crate::extract::social_site(&post), Some("danbooru"));
        assert_eq!(
            target(Site::Danbooru, &post).unwrap(),
            (
                Target::Post { id: 22 },
                Url::parse(&format!("https://{host}/")).unwrap()
            ),
            "{host}"
        );
        let search = Url::parse(&format!("https://{host}/posts?tags=a+b")).unwrap();
        assert_eq!(
            source_search_tags(Site::Danbooru, &search).as_deref(),
            Some("a b")
        );
        let defaults = defaults_for_url(&search).unwrap();
        assert_eq!(
            (defaults.category, defaults.archive),
            ("danbooru", "t_{search_tags}_{id}")
        );
        let descriptor = crate::source_archive::for_url(&search).unwrap();
        assert_eq!(descriptor.category, "danbooru");
        assert_eq!(descriptor.format, "t_{search_tags}_{id}");
    }
    // The existing native shorthands keep danbooru.donmai.us as their root.
    for raw in [
        "https://donmai.us/posts/1",
        "https://www.danbooru.donmai.us/posts/1",
    ] {
        assert_eq!(
            t(Site::Danbooru, raw).unwrap().1.as_str(),
            "https://danbooru.donmai.us/"
        );
    }
    // Anything outside upstream's alternation is not the danbooru instance.
    for raw in [
        "https://www.hijiribe.donmai.us/posts/1",
        "https://www.donmai.moe/posts/1",
        "https://evil.donmai.moe/posts/1",
        "https://testbooru.donmai.us/posts/1",
        "https://donmai.moe.example/posts/1",
        "https://hijiribe.donmai.us:8443/posts/1",
    ] {
        assert!(t(Site::Danbooru, raw).is_err(), "{raw}");
    }
    assert!(!danbooru_host("donmai.us"));
    // Files from donmai.moe's own hosts are accepted like cdn.donmai.us.
    let media = parse_post(
        Site::Danbooru,
        &danbooru_dialect_post(1, &format!("https://cdn.donmai.moe/original/{MD5}.png")),
    )
    .unwrap();
    assert_eq!(media.category, "danbooru");
}

#[tokio::test]
async fn danbooru_alias_hosts_extract_as_danbooru() {
    let cdn = format!("https://cdn.donmai.us/original/01/23/{MD5}.png");
    let body = danbooru_dialect_post(9, &cdn).to_string();
    let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
    let events: Vec<_> = booru(Site::Danbooru, root)
        .events(&Url::parse("https://safebooru.donmai.us/posts/9").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    assert_eq!(files[0].category, "danbooru");
    assert_eq!(
        paths(files[0]),
        (format!("danbooru_9_{MD5}.png"), "danbooru".into())
    );
    assert!(files[0].archive_key().unwrap().starts_with("danbooru:"));
    let requests = server.join().unwrap();
    assert!(
        requests[0].starts_with("GET /posts/9.json"),
        "{}",
        requests[0]
    );
}

#[test]
fn e926_is_its_own_e621_dialect_instance() {
    assert_eq!(Site::from_category("e926"), Some(Site::E926));
    assert_eq!(Site::E926.dialect(), Dialect::E621);
    assert_eq!(category_dialect("e926"), Some(Dialect::E621));
    let url = Url::parse("https://e926.net/posts?tags=cat").unwrap();
    assert_eq!(crate::extract::social_site(&url), Some("e926"));
    assert_eq!(site_for_url(&url), Some(Site::E926));
    assert_eq!(
        target(Site::E926, &url).unwrap(),
        (
            Target::Tags { tags: "cat".into() },
            Url::parse("https://e926.net/").unwrap()
        )
    );
    assert_eq!(source_search_tags(Site::E926, &url).as_deref(), Some("cat"));
    // Neither instance accepts the other's host.
    assert!(t(Site::E621, "https://e926.net/posts/1").is_err());
    assert!(t(Site::E926, "https://e621.net/posts/1").is_err());
    assert!(t(Site::E926, "https://e621.cc/?tags=cat").is_err());
    assert_eq!(source_search_tags(Site::E621, &url), None);
    // e621.net output keeps its category and identity.
    let e621 = parse_post(
        Site::E621,
        &e6ai_post(
            5,
            json!(format!("https://static1.e621.net/data/01/23/{MD5}.png")),
        ),
    )
    .unwrap();
    assert_eq!(e621.category, "e621");
    assert!(e621.archive_key().unwrap().starts_with("e621:"));
    // e926 accepts e621's CDN and its own, keyed by its own category.
    let e926 = parse_post(
        Site::E926,
        &e6ai_post(
            5,
            json!(format!("https://static1.e621.net/data/01/23/{MD5}.png")),
        ),
    )
    .unwrap();
    assert_eq!(e926.category, "e926");
    assert_eq!(e926.filename, format!("e926_5_{MD5}.png"));
    assert!(e926.archive_key().unwrap().starts_with("e926:"));
    assert_ne!(e926.archive_key(), e621.archive_key());
    // Hidden e926 files are rebuilt on upstream's `static1.{root host}`.
    let hidden = parse_post(Site::E926, &e6ai_post(5, Value::Null)).unwrap();
    assert_eq!(
        hidden.url.as_str(),
        format!("https://static1.e926.net/data/01/23/{MD5}.png")
    );
    let hidden = parse_post(Site::E621, &e6ai_post(5, Value::Null)).unwrap();
    assert_eq!(
        hidden.url.as_str(),
        format!("https://static1.e621.net/data/01/23/{MD5}.png")
    );
}

#[tokio::test]
async fn e926_extracts_under_its_own_category_and_credentials() {
    let body = json!({"post": e6ai_post(5, Value::Null)}).to_string();
    let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
    let events: Vec<_> = booru(Site::E926, root)
        .with_credentials(Some("nine"), Some("nine-key"))
        .unwrap()
        .events(&Url::parse("https://e926.net/posts/5").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    assert_eq!(files[0].category, "e926");
    assert_eq!(
        paths(files[0]),
        (format!("e926_5_{MD5}.png"), "e926".into())
    );
    assert_eq!(crate::postprocess::context(files[0])["category"], "e926");
    let descriptor = crate::source_archive::for_media(files[0]).unwrap();
    assert_eq!(
        (descriptor.category.as_str(), descriptor.format.as_str()),
        ("e926", "{id}")
    );
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /posts/5.json "));
    use base64::Engine;
    let basic = base64::engine::general_purpose::STANDARD.encode("nine:nine-key");
    assert!(requests[0].to_ascii_lowercase().contains(&format!(
        "authorization: basic {}",
        basic.to_ascii_lowercase()
    )));
}

/// Register a configured instance (the registry is process-wide: unique
/// names and hosts per test).
fn define(base: crate::base_instances::Base, category: &str, value: Value) -> Site {
    crate::base_instances::register(&[crate::base_instances::Definition::parse(
        base, category, &value,
    )
    .unwrap()
    .unwrap()]);
    Site::from_category(category).unwrap()
}

#[test]
fn configured_instances_use_their_family_root_and_own_identity() {
    use crate::base_instances::Base;
    let site = define(
        Base::Danbooru,
        "cfgbooru",
        json!({"root": "https://cfgbooru.test"}),
    );
    assert_eq!(site.category(), "cfgbooru");
    assert_eq!(site.dialect(), Dialect::Danbooru);
    assert_eq!(site.slot(), None);
    assert!(is_danbooru_family("cfgbooru"));
    let url = Url::parse("https://cfgbooru.test/posts?tags=a+b").unwrap();
    assert_eq!(crate::extract::social_site(&url), Some("cfgbooru"));
    assert_eq!(site_for_url(&url), Some(site));
    assert_eq!(
        target(site, &url).unwrap(),
        (
            Target::Tags { tags: "a b".into() },
            Url::parse("https://cfgbooru.test/").unwrap()
        )
    );
    assert_eq!(source_search_tags(site, &url).as_deref(), Some("a b"));
    let defaults = defaults_for_url(&url).unwrap();
    assert_eq!(
        (defaults.category, defaults.archive),
        ("cfgbooru", "t_{search_tags}_{id}")
    );
    let descriptor = crate::source_archive::for_url(&url).unwrap();
    assert_eq!(descriptor.category, "cfgbooru");
    // Without a pattern only the root's exact host matches.
    for raw in [
        "https://www.cfgbooru.test/posts/1",
        "https://cfgbooru.test:8443/posts/1",
        "https://danbooru.donmai.us/posts/1",
    ] {
        assert!(t(site, raw).is_err(), "{raw}");
    }
    assert!(t(Site::Danbooru, "https://cfgbooru.test/posts/1").is_err());
    // Files come from the root's domain; root-relative ones resolve against it.
    for (raw, expected) in [
        (
            format!("https://cdn.cfgbooru.test/{MD5}.png"),
            format!("https://cdn.cfgbooru.test/{MD5}.png"),
        ),
        (
            format!("/data/{MD5}.png"),
            format!("https://cfgbooru.test/data/{MD5}.png"),
        ),
    ] {
        let media = parse_post(site, &danbooru_dialect_post(3, &raw)).unwrap();
        assert_eq!(media.url.as_str(), expected);
        assert_eq!(media.category, "cfgbooru");
        assert!(media.archive_key().unwrap().starts_with("cfgbooru:"));
    }
    for foreign in [
        format!("https://cdn.donmai.us/original/{MD5}.png"),
        format!("http://cdn.cfgbooru.test/{MD5}.png"),
    ] {
        assert!(
            parse_post(site, &danbooru_dialect_post(3, &foreign)).is_none(),
            "{foreign}"
        );
    }
    // Configured e621-family instances: pattern routing and the
    // `static1.{root host}` hidden-file host.
    let six = define(
        Base::E621,
        "cfg621",
        json!({"root": "https://cfg621.test", "pattern": r"(?:www\.)?cfg621\.(?:test|example)"}),
    );
    assert_eq!(six.dialect(), Dialect::E621);
    assert_eq!(
        t(six, "https://www.cfg621.example/posts/9").unwrap(),
        (
            Target::Post { id: 9 },
            Url::parse("https://cfg621.test/").unwrap()
        )
    );
    let hidden = parse_post(six, &e6ai_post(4, Value::Null)).unwrap();
    assert_eq!(
        hidden.url.as_str(),
        format!("https://static1.cfg621.test/data/01/23/{MD5}.png")
    );
    assert_eq!(hidden.category, "cfg621");
}

#[tokio::test]
async fn configured_and_prefixed_instances_extract_with_their_credentials() {
    use crate::base_instances::Base;
    let cdn = format!("https://cdn.credbooru.test/{MD5}.png");
    let page = json!([danbooru_dialect_post(7, &cdn)]).to_string();
    let (root, server) = serve(vec![response("200 OK", "", page.as_bytes())]);
    let site = define(
        Base::Danbooru,
        "credbooru",
        json!({"root": "https://credbooru.test"}),
    );
    let events: Vec<_> = booru(site, root)
        .with_credentials(Some("cred-user"), Some("cred-key"))
        .unwrap()
        .events(&Url::parse("https://credbooru.test/posts?tags=cat").unwrap())
        .collect()
        .await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    assert_eq!(
        paths(files[0]),
        (format!("credbooru_7_{MD5}.png"), "credbooru/cat".into())
    );
    assert!(files[0].headers.is_empty());
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /posts.json?tags=cat&limit=200"));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("authorization: basic ")
    );
    // `Danbooru:URL` inputs use the family for any host, with the host as
    // category and root.
    let input = crate::extract::input_url("Danbooru:https://prebooru.test/posts/5").unwrap();
    assert_eq!(crate::extract::social_site(&input), Some("prebooru.test"));
    let url = crate::base_instances::unprefix(input).unwrap();
    let site = site_for_url(&url).unwrap();
    assert_eq!(site.category(), "prebooru.test");
    assert!(site.instance().unwrap().prefixed);
    assert_eq!(
        target(site, &url).unwrap(),
        (
            Target::Post { id: 5 },
            Url::parse("https://prebooru.test/").unwrap()
        )
    );
    let body = danbooru_dialect_post(5, &format!("/data/{MD5}.png")).to_string();
    let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
    let events: Vec<_> = booru(site, root).events(&url).collect().await;
    let files = media(&events);
    assert_eq!(files.len(), 1, "{events:?}");
    assert_eq!(
        files[0].url.as_str(),
        format!("https://prebooru.test/data/{MD5}.png")
    );
    assert_eq!(
        paths(files[0]),
        (format!("prebooru.test_5_{MD5}.png"), "prebooru.test".into())
    );
    assert!(server.join().unwrap()[0].starts_with("GET /posts/5.json"));
}
