//! filester.me files and folders, mirroring gallery_dl/extractor/filester.py:
//! each download URL comes from `POST /api/public/download`.
use super::Extr;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "filester",
    name: "Filester",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE, &FOLDER],
    api_key: false,
    cookies: None,
};

static FILE: Route = Route::new(
    "file",
    "{id} {filename}.{extension}",
    &["{category}"],
    "{id}",
);
static FOLDER: Route = Route::new(
    "folder",
    "{num:>03} {id} {filename}.{extension}",
    &["{category}", "{folder_name} ({folder_id})"],
    "{id}",
);

const ROOT: &str = "https://filester.me";

#[derive(Debug, PartialEq)]
enum Target {
    File(String),
    Folder(String),
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 128
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn target(url: &Url) -> Option<Target> {
    if !super::on_host(url, &["filester.me"]) {
        return None;
    }
    match crate::sites::segments(url).as_slice() {
        [d, slug, ..] if d == "d" && valid_slug(slug) => Some(Target::File(slug.clone())),
        [f, slug, ..] if f == "f" && valid_slug(slug) => Some(Target::Folder(slug.clone())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::File(slug)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/d/{slug}"))?).await?;
            let (file, timestamp) = parse_file(&page, &slug);
            let url = download_url(http, &slug).await?;
            let item = Item::new(url, file)
                .ids(slug.clone(), slug)
                .timestamp(timestamp);
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &FILE,
                vec![item],
            )))
        }),
        Some(Target::Folder(slug)) => folder(http, slug),
        None => crate::sites::fail(error("unsupported filester URL")),
    }
}

fn info<'t>(extr: &mut Extr<'t>, label: &str) -> &'t str {
    super::rpartition(extr.next(&format!(">{label}</span"), "</span"), ">").2
}

fn parse_file(page: &str, slug: &str) -> (Map<String, Value>, Option<i64>) {
    let mut extr = Extr::new(page);
    let name = crate::sites::percent_decode(&crate::sites::unescape(
        extr.next("property=\"og:title\" content=\"", "\""),
    ));
    let mut file = Map::new();
    file.insert("id".into(), slug.into());
    file.insert("uuid".into(), info(&mut extr, "UUID").into());
    file.insert("mime".into(), info(&mut extr, "Type").into());
    let timestamp = super::parse_iso(info(&mut extr, "Uploaded"));
    file.insert("date".into(), super::date_value(timestamp));
    file.insert("size".into(), info(&mut extr, "Size").into());
    file.insert("hash".into(), info(&mut extr, "SHA-256").into());
    super::nameext_from_name(&name, &mut file);
    (file, timestamp)
}

/// `_download_url`.
async fn download_url(http: &Http, slug: &str) -> Result<Url> {
    let data = http
        .post_json(
            &http.api(&format!("{ROOT}/api/public/download"))?,
            &json!({"file_slug": slug}),
            &[],
        )
        .await?;
    let path = data
        .get("download_url")
        .and_then(Value::as_str)
        .filter(|p| p.starts_with('/') && !p.starts_with("//"))
        .ok_or_else(|| error("filester returned no download URL"))?;
    let cache = [1, 6][super::random_index(2)];
    Url::parse(&format!(
        "https://cache{cache}.filester.me{path}?download=true"
    ))
    .map_err(|_| error("filester returned an invalid download URL"))
}

/// First-page folder metadata, merged into every file as upstream's kwdict.
fn folder_metadata(page: &str, slug: &str) -> Map<String, Value> {
    let mut extr = Extr::new(page);
    let mut kw = Map::new();
    kw.insert("folder_id".into(), slug.into());
    kw.insert(
        "folder_name".into(),
        crate::sites::unescape(extr.next("property=\"og:title\" content=\"", "\"")).into(),
    );
    kw.insert("folder_uuid".into(), extr.next("/t/", "\"").into());
    kw.insert(
        "folder_date".into(),
        super::date_value(super::strptime(
            extr.next("<span>Created ", "<"),
            "%b %d, %Y",
        )),
    );
    kw.insert(
        "count".into(),
        super::parse_int(extr.next("<span>", " ")).into(),
    );
    kw.insert(
        "folder_size".into(),
        super::parse_bytes(extr.next("<span>", "B<")).into(),
    );
    kw
}

struct Entry {
    file: Map<String, Value>,
    timestamp: Option<i64>,
}

fn parse_entries(page: &str, num: &mut i64) -> Vec<Entry> {
    super::extract_iter(page, "class=\"file-item\"", "</button>")
        .into_iter()
        .map(|html| {
            let mut extr = Extr::new(html);
            let name = crate::sites::unescape(extr.next("data-name=\"", "\""));
            let mut file = Map::new();
            file.insert("size".into(), extr.next("data-size=\"", "\"").into());
            let timestamp = super::parse_iso(extr.next("data-date=\"", "\""));
            file.insert("date".into(), super::date_value(timestamp));
            file.insert("id".into(), extr.next("href='/d/", "'").into());
            file.insert("uuid".into(), extr.next("src=\"/t/", "\"").into());
            file.insert("num".into(), (*num).into());
            *num -= 1;
            super::nameext_from_name(&name, &mut file);
            Entry { file, timestamp }
        })
        .collect()
}

fn folder(http: &Http, slug: String) -> Events<'_> {
    struct State {
        page: u64,
        kw: Option<Map<String, Value>>,
        num: i64,
    }
    let start = State {
        page: 1,
        kw: None,
        num: 0,
    };
    crate::sites::paginate(Some(start), move |state| {
        let slug = slug.clone();
        async move {
            let Some(mut state) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/f/{slug}"))?;
            url.query_pairs_mut()
                .append_pair("page", &state.page.to_string());
            let page = http.text(&url).await?;
            let kw = state
                .kw
                .get_or_insert_with(|| {
                    let kw = folder_metadata(&page, &slug);
                    state.num = kw.get("count").and_then(Value::as_i64).unwrap_or(0);
                    kw
                })
                .clone();
            let entries = parse_entries(&page, &mut state.num);
            let events = super::each(&SITE, http, &FOLDER, entries, move |entry| {
                let kw = kw.clone();
                async move {
                    let id = entry
                        .file
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if !valid_slug(&id) {
                        return Err(error("filester folder entry has no valid ID"));
                    }
                    let url = download_url(http, &id).await?;
                    let mut file = entry.file;
                    for (key, value) in &kw {
                        file.insert(key.clone(), value.clone());
                    }
                    let folder = kw
                        .get("folder_id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default();
                    Ok(Some(
                        Item::new(url, file)
                            .ids(folder, id)
                            .timestamp(entry.timestamp),
                    ))
                }
            });
            let next = (page.contains(">→</a>") && state.page < 10_000).then(|| {
                state.page += 1;
                state
            });
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://filester.me/d/AbC-1"),
            Some(Target::File("AbC-1".into()))
        );
        assert_eq!(
            t("https://www.filester.me/f/xyz"),
            Some(Target::Folder("xyz".into()))
        );
        for raw in [
            "https://filester.me/x/abc",
            "https://filester.me.evil.test/d/abc",
            "https://cache1.filester.me/d/abc",
            "https://filester.me:80/d/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_file() {
        let (root, server) = serve(vec![
            html(
                "<meta property=\"og:title\" content=\"My%20Doc.PDF\">\
<span>UUID</span><span>u-1</span><span>Type</span><span>application/pdf</span>\
<span>Uploaded</span><span>2024-06-05T00:00:00Z</span><span>Size</span><span>1.2 MB</span>\
<span>SHA-256</span><span>abc</span>",
            ),
            json(serde_json::json!({"download_url": "/dl/token/My%20Doc.PDF"})),
        ]);
        let http = http("Filester", root);
        let files = media(extract(
            &http,
            Url::parse("https://filester.me/d/S1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert!(files[0].url.as_str().starts_with("https://cache"));
        assert!(
            files[0]
                .url
                .as_str()
                .ends_with(".filester.me/dl/token/My%20Doc.PDF?download=true")
        );
        let k = keywords(&files[0]);
        assert_eq!(k["uuid"], "u-1");
        assert_eq!(k["mime"], "application/pdf");
        assert_eq!(k["size"], "1.2 MB");
        assert_eq!(k["hash"], "abc");
        assert_eq!(k["date"], "2024-06-05 00:00:00");
        assert_eq!(k["filename"], "My Doc");
        assert_eq!(k["extension"], "pdf");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "S1 My Doc.pdf");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("filester")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["S1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /d/S1 "));
        assert!(requests[1].starts_with("POST /api/public/download "));
        assert!(requests[1].ends_with("{\"file_slug\":\"S1\"}"));
    }

    fn entry(name: &str, id: &str) -> String {
        format!(
            "<div class=\"file-item\" data-name=\"{name}\" data-size=\"10\" \
             data-date=\"2024-01-01T00:00:00Z\"><a href='/d/{id}'><img src=\"/t/uu{id}\"></button>"
        )
    }

    #[tokio::test]
    async fn paginates_folder() {
        let first = format!(
            "<meta property=\"og:title\" content=\"Stuff &amp; things\"><img src=\"/t/FU\">\
             <span>Created Jun 5, 2024</span><span>2 files</span><span>3.5 KB</span>{}<a>→</a>",
            entry("a.png", "A")
        );
        let (root, server) = serve(vec![
            html(&first),
            json(serde_json::json!({"download_url": "/a"})),
            html(&entry("b.png", "B")),
            json(serde_json::json!({"download_url": "/b"})),
        ]);
        let http = http("Filester", root);
        let files = media(extract(
            &http,
            Url::parse("https://filester.me/f/FID").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["folder_name"], "Stuff & things");
        assert_eq!(k["folder_uuid"], "FU");
        assert_eq!(k["folder_date"], "2024-06-05 00:00:00");
        assert_eq!(k["folder_size"], 3584);
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["uuid"], "uuA");
        assert_eq!(keywords(&files[1])["num"], 1);
        assert_eq!(keywords(&files[1])["folder_name"], "Stuff & things");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "002 A a.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("filester/Stuff & things (FID)")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /f/FID?page=1 "));
        assert!(requests[2].starts_with("GET /f/FID?page=2 "));
    }

    #[tokio::test]
    async fn reports_missing_download_url() {
        let (root, _server) = serve(vec![
            html("<meta property=\"og:title\" content=\"x.txt\">"),
            json(serde_json::json!({"detail": "gone"})),
        ]);
        let http = http("Filester", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://filester.me/d/S1").unwrap(),
        ))
        .await;
        assert!(
            events[0]
                .as_ref()
                .err()
                .unwrap()
                .to_string()
                .contains("no download URL")
        );
    }
}
