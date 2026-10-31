//! Bunkr albums and media pages, mirroring gallery_dl/extractor/bunkr.py
//! (a lolisafe derivative): album pages list files whose download URLs come
//! from the `apidl` endpoint, XOR-decrypted when flagged as encrypted.
use super::{Body, Extr, request};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "bunkr",
    name: "Bunkr",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &MEDIA],
    api_key: false,
    cookies: None,
};

static ALBUM: Route = Route::new(
    "album",
    "{filename}.{extension}",
    &["{category}", "{album_name} ({album_id})"],
    "{album_id}_{id|id_url|slug}",
);
static MEDIA: Route = Route::new(
    "media",
    "{filename}.{extension}",
    &["{category}"],
    "{album_id}_{id|id_url|slug}",
);

const ROOT_DL: &str = "https://get.bunkrr.su";
const ROOT_API: &str = "https://apidl.bunkr.ru";
const DOMAINS: &[&str] = &[
    "bunkr.ac",
    "bunkr.ci",
    "bunkr.cr",
    "bunkr.fi",
    "bunkr.ph",
    "bunkr.pk",
    "bunkr.ps",
    "bunkr.si",
    "bunkr.sk",
    "bunkr.ws",
    "bunkr.black",
    "bunkr.red",
    "bunkr.media",
    "bunkr.site",
];
const LEGACY_DOMAINS: &[&str] = &[
    "bunkr.ax",
    "bunkr.cat",
    "bunkr.ru",
    "bunkrr.ru",
    "bunkr.su",
    "bunkrr.su",
    "bunkr.la",
    "bunkr.is",
    "bunkr.to",
];
const TLDS: &[&str] = &[
    "sk", "si", "su", "ci", "cr", "fi", "ph", "pk", "ps", "ru", "la", "is", "to", "ac", "ax",
    "black", "cat", "media", "red", "site", "ws", "org",
];

#[derive(Debug, PartialEq)]
enum Target {
    Album { domain: String, id: String },
    Media { domain: String, path: String },
}

/// Upstream `extractor.bunkr.tlds`: accept every top-level domain.
pub static ANY_TLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `bunkr+\.(TLD)`, optionally behind `app.`; returns the domain group.
fn domain(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let host = host.strip_prefix("app.").unwrap_or(host);
    let (name, tld) = host.split_once('.')?;
    let rs = name.strip_prefix("bunk")?;
    let any = ANY_TLD.load(std::sync::atomic::Ordering::Relaxed)
        && !tld.is_empty()
        && tld.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    (!rs.is_empty() && rs.bytes().all(|b| b == b'r') && (any || TLDS.contains(&tld)))
        .then(|| host.to_owned())
}

fn target(url: &Url) -> Option<Target> {
    // `bunkr:[https://]HOST/...` selects a custom domain.
    if let Some(inner) = crate::sites::custom_domain(url, "bunkr") {
        let domain = inner.host_str()?.to_owned();
        return target_on(&inner, domain);
    }
    if !crate::sites::plain(url) {
        return None;
    }
    target_on(url, domain(url)?)
}

fn target_on(url: &Url, domain: String) -> Option<Target> {
    let parts = crate::sites::segments(url);
    match parts.as_slice() {
        [a, id, ..] if a == "a" => Some(Target::Album {
            domain,
            id: id.clone(),
        }),
        [kind, id, ..] if matches!(kind.as_str(), "f" | "v" | "i" | "d") => Some(Target::Media {
            domain,
            path: format!("/{kind}/{id}"),
        }),
        _ => None,
    }
}

fn root_for(domain: &str) -> String {
    if LEGACY_DOMAINS.contains(&domain) {
        "https://bunkr.si".into()
    } else {
        format!("https://{domain}")
    }
}

fn endpoint(http: &Http) -> Result<Url> {
    let raw = match http.option_str("endpoint").filter(|e| !e.is_empty()) {
        None => format!("{ROOT_API}/api/_001_v2"),
        Some(path) if path.starts_with('/') => format!("{ROOT_API}{path}"),
        Some(full) => full.to_owned(),
    };
    super::web_url(&raw)
        .ok_or_else(|| error("invalid bunkr endpoint option"))
        .and_then(|url| http.api(url.as_str()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album { domain, id }) => {
            crate::sites::once(async move { album(http, &root_for(&domain), &id).await })
        }
        Some(Target::Media { domain, path }) => {
            crate::sites::once(async move { media(http, &root_for(&domain), &path).await })
        }
        None => crate::sites::fail(error("unsupported bunkr URL")),
    }
}

/// Fetch a page, following redirects to other bunkr domains and falling
/// back to alternative domains when one refuses access.
async fn page(http: &Http, root: &str, path: &str) -> Result<String> {
    let mut candidates: Vec<String> = vec![root.to_owned()];
    let start = super::random_index(DOMAINS.len());
    for i in 0..3 {
        let fallback = format!("https://{}", DOMAINS[(start + i) % DOMAINS.len()]);
        if !candidates.contains(&fallback) {
            candidates.push(fallback);
        }
    }
    let mut last = error("bunkr page request failed");
    for root in candidates {
        match follow(http, &format!("{root}{path}")).await {
            Ok(text) => return Ok(text),
            // Upstream treats HTTP 403 as a Cloudflare challenge and tries
            // another domain; other failures are final.
            Err(e) if e.to_string().contains("refused access") => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

async fn follow(http: &Http, start: &str) -> Result<String> {
    let mut url = Url::parse(start).map_err(|_| error("invalid bunkr URL"))?;
    for _ in 0..6 {
        let response = request(
            http,
            reqwest::Method::GET,
            &http.api(url.as_str())?,
            &[],
            Body::None,
        )
        .await?;
        if response.status < 300 {
            return Ok(response.text());
        }
        let location = response
            .location
            .ok_or_else(|| error("bunkr redirect without Location"))?;
        let next = url
            .join(&location)
            .map_err(|_| error("bunkr redirect has invalid Location"))?;
        if !crate::sites::plain(&next) || domain(&next).is_none() {
            return Err(error("bunkr redirected to an unexpected host"));
        }
        url = next;
    }
    Err(error("bunkr redirect limit exceeded"))
}

fn og_title(page: &str) -> String {
    crate::sites::unescape(&crate::sites::unescape(
        crate::sites::extr(page, "property=\"og:title\" content=\"", "\"").unwrap_or_default(),
    ))
}

fn album_size(page: &str) -> String {
    crate::sites::extr(page, "<span class=\"font-semibold\">(", ")")
        .unwrap_or_default()
        .to_owned()
}

/// One `window.albumFiles` entry's fields, before the API lookup.
#[derive(Debug, Default)]
struct Entry {
    data_id: String,
    name: Value,
    slug: Value,
    uuid: String,
    size: i64,
    date: Option<i64>,
}

fn json_literal(item: &str, begin: &str) -> Value {
    let raw = crate::sites::extr(item, begin, ",\n").unwrap_or_default();
    serde_json::from_str(raw.trim().replace("\\'", "'").as_str()).unwrap_or(Value::Null)
}

fn parse_album(page: &str, album_id: &str) -> (Vec<Entry>, Map<String, Value>) {
    let files = crate::sites::extr(page, "window.albumFiles = [", "</script>").unwrap_or_default();
    let items: Vec<&str> = files.split("\n},\n").collect();
    let entries = items
        .iter()
        .map(|item| Entry {
            data_id: crate::sites::extr(item, " id: ", ",")
                .unwrap_or_default()
                .trim()
                .to_owned(),
            name: json_literal(item, "original:"),
            slug: json_literal(item, "slug: "),
            uuid: crate::sites::extr(item, "name: \"", ".")
                .unwrap_or_default()
                .to_owned(),
            size: super::parse_int(crate::sites::extr(item, "size:  ", " ,\n").unwrap_or_default()),
            date: super::strptime(
                crate::sites::extr(item, "timestamp: \"", "\"").unwrap_or_default(),
                "%H:%M:%S %d/%m/%Y",
            ),
        })
        .collect();
    let mut data = Map::new();
    data.insert("album_id".into(), album_id.into());
    data.insert("album_name".into(), og_title(page).into());
    data.insert("album_size".into(), album_size(page).into());
    data.insert("count".into(), items.len().into());
    (entries, data)
}

/// The decrypted download URL from an `apidl` response.
fn file_url(data: &Value) -> Result<Url> {
    let raw = data
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| error("bunkr API response has no URL"))?;
    let raw = if data.get("encrypted").and_then(Value::as_bool) == Some(true) {
        let hour = data.get("timestamp").and_then(Value::as_i64).unwrap_or(0) / 3600;
        super::decrypt_xor(raw, format!("SECRET_KEY_{hour}").as_bytes(), false)
            .ok_or_else(|| error("could not decrypt bunkr file URL"))?
    } else {
        raw.to_owned()
    };
    super::web_url(&raw).ok_or_else(|| error("bunkr returned an invalid file URL"))
}

async fn resolve(http: &Http, data_id: &str) -> Result<(Url, String)> {
    if data_id.is_empty() || data_id.len() > 64 {
        return Err(error("bunkr file entry has no ID"));
    }
    let referer = format!("{ROOT_DL}/file/{data_id}");
    let data = http
        .post_json(
            &endpoint(http)?,
            &json!({"id": data_id}),
            &[("Referer", &referer), ("Origin", ROOT_DL)],
        )
        .await?;
    Ok((file_url(&data)?, referer))
}

/// The lolisafe `items()` naming rules plus upstream file fields.
fn file_item(
    url: Url,
    referer: &str,
    data_id: &str,
    entry: Entry,
    data: &Map<String, Value>,
    num: usize,
) -> Item {
    let mut file = Map::new();
    file.insert("file".into(), url.as_str().into());
    file.insert("id_url".into(), data_id.into());
    file.insert("name".into(), entry.name.clone());
    file.insert("slug".into(), entry.slug);
    file.insert("uuid".into(), entry.uuid.into());
    file.insert("size".into(), entry.size.into());
    file.insert("date".into(), super::date_value(entry.date));
    for (key, value) in data {
        file.insert(key.clone(), value.clone());
    }
    file.insert("num".into(), num.into());
    let (stem, ext) = crate::sites::nameext(&url);
    file.insert("filename".into(), stem.clone().into());
    file.insert("extension".into(), ext.into());
    match &entry.name {
        Value::String(name) => {
            let base = match super::rpartition(name, ".").0 {
                "" => name.clone(),
                base => base.to_owned(),
            };
            file.insert("name".into(), base.clone().into());
            let (_, sep, fid) = super::rpartition(&stem, "-");
            if sep.is_empty() || fid.chars().count() == 12 {
                file.insert("id".into(), "".into());
                file.insert("filename".into(), base.into());
            } else {
                file.insert("id".into(), fid.into());
                file.insert("filename".into(), format!("{base}-{fid}").into());
            }
        }
        _ => {
            // Upstream's `name` branch needs a string; without one it falls
            // back to splitting the URL filename.
            let (name, _, id) = super::rpartition(&stem, "-");
            file.insert("name".into(), name.into());
            file.insert("id".into(), id.into());
        }
    }
    let album = data
        .get("album_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let caption = file.get("name").and_then(Value::as_str).map(str::to_owned);
    Item::new(url, file)
        .ids(album, data_id)
        .caption(caption)
        .timestamp(entry.date)
        .header("Referer", referer)
}

async fn album<'a>(http: &'a Http, root: &str, id: &str) -> Result<Events<'a>> {
    let text = page(http, root, &format!("/a/{id}?advanced=1")).await?;
    let (entries, data) = parse_album(&text, id);
    let inputs: Vec<(usize, Entry)> = entries.into_iter().enumerate().collect();
    Ok(super::each(
        &SITE,
        http,
        &ALBUM,
        inputs,
        move |(index, entry)| {
            let data = data.clone();
            async move {
                let data_id = entry.data_id.clone();
                let (url, referer) = resolve(http, &data_id).await?;
                Ok(Some(file_item(
                    url,
                    &referer,
                    &data_id,
                    entry,
                    &data,
                    index + 1,
                )))
            }
        },
    ))
}

async fn media<'a>(http: &'a Http, root: &str, path: &str) -> Result<Events<'a>> {
    let text = page(http, root, path).await?;
    let mut extr = Extr::new(&text);
    let data_id = crate::sites::extr(&text, "data-file-id=\"", "\"")
        .unwrap_or_default()
        .to_owned();
    let (url, referer) = resolve(http, &data_id).await?;
    let heading = super::rpartition(extr.next("<h1", "<"), ">").2;
    let entry = Entry {
        data_id: data_id.clone(),
        name: crate::sites::percent_decode(&crate::sites::unescape(heading)).into(),
        slug: super::rpartition(path, "/").2.into(),
        uuid: crate::sites::extr(&text, "/thumbs/", ".")
            .unwrap_or_default()
            .to_owned(),
        size: 0,
        date: None,
    };
    let album_id = crate::sites::extr(&text, " href=\"../a/", "\"").unwrap_or_default();
    let (album_name, album_size) = if album_id.is_empty() {
        (String::new(), Value::from(-1))
    } else {
        match page(http, root, &format!("/a/{album_id}")).await {
            Ok(album) => (og_title(&album), album_size(&album).into()),
            Err(_) => (String::new(), Value::from(-1)),
        }
    };
    let mut data = Map::new();
    data.insert("album_id".into(), album_id.into());
    data.insert("album_name".into(), album_name.into());
    data.insert("album_size".into(), album_size);
    data.insert("count".into(), 1.into());
    let mut item = file_item(url, &referer, &data_id, entry, &data, 1);
    // Media pages carry no size or upload time.
    item.keywords.remove("size");
    item.keywords.remove("date");
    Ok(crate::sites::events(crate::sites::post(
        &SITE,
        http,
        &MEDIA,
        vec![item],
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media as collect, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for raw in [
            "bunkr:http://example.org/a/Lktg9Keq",
            "bunkr:example.org/a/Lktg9Keq",
        ] {
            assert_eq!(
                t(raw),
                Some(Target::Album {
                    domain: "example.org".into(),
                    id: "Lktg9Keq".into()
                }),
                "{raw}"
            );
        }
        assert_eq!(
            t("https://bunkr.si/a/Lktg9Keq"),
            Some(Target::Album {
                domain: "bunkr.si".into(),
                id: "Lktg9Keq".into()
            })
        );
        assert_eq!(
            t("https://app.bunkrrr.cr/a/X"),
            Some(Target::Album {
                domain: "bunkrrr.cr".into(),
                id: "X".into()
            })
        );
        assert_eq!(
            t("https://bunkr.black/f/name-AbCd.mp4"),
            Some(Target::Media {
                domain: "bunkr.black".into(),
                path: "/f/name-AbCd.mp4".into()
            })
        );
        for raw in [
            "https://bunkr.com/a/X",
            "https://bunk.si/a/X",
            "https://www.bunkr.si/a/X",
            "https://bunkr.si.evil.test/a/X",
            "https://evilbunkr.si/a/X",
            "https://bunkr.si:444/a/X",
            "https://bunkr.si/x/X",
            "https://bunkr.si/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(root_for("bunkr.su"), "https://bunkr.si");
        assert_eq!(root_for("bunkr.cr"), "https://bunkr.cr");
    }

    const ALBUM_PAGE: &str = "<meta property=\"og:title\" content=\"My &amp;amp; Album\">\n\
<span class=\"font-semibold\">(12.5 MB)</span>\n\
<script>window.albumFiles = [\n{\n id: 111,\n name: \"uuid1.jpg\",\n original: \"Cat's pic.jpg\",\n slug: \"cat-pic-AbCdEfGh\",\n size:  1234 ,\n timestamp: \"12:30:00 05/06/2024\",\n},\n{\n id: 222,\n name: \"uuid2.mp4\",\n original: \"clip.mp4\",\n slug: \"clip-XyZ\",\n size:  99 ,\n timestamp: \"bad\",\n}];\n</script>";

    #[test]
    fn parses_album_entries() {
        let (entries, data) = parse_album(ALBUM_PAGE, "AID");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].data_id, "111");
        assert_eq!(entries[0].name, "Cat's pic.jpg");
        assert_eq!(entries[0].slug, "cat-pic-AbCdEfGh");
        assert_eq!(entries[0].uuid, "uuid1");
        assert_eq!(entries[0].size, 1234);
        assert_eq!(entries[0].date, Some(1_717_590_600));
        assert_eq!(entries[1].date, None);
        assert_eq!(data["album_name"], "My & Album");
        assert_eq!(data["album_size"], "12.5 MB");
        assert_eq!(data["count"], 2);
    }

    #[test]
    fn decrypts_file_urls() {
        use base64::Engine;
        let key = b"SECRET_KEY_2";
        let xored: Vec<u8> = b"https://c.bunkr.ru/x-AbCd.jpg"
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % key.len()])
            .collect();
        let data = json!({"encrypted": true, "timestamp": 7200 + 5,
            "url": base64::engine::general_purpose::STANDARD.encode(xored)});
        assert_eq!(
            file_url(&data).unwrap().as_str(),
            "https://c.bunkr.ru/x-AbCd.jpg"
        );
        assert!(file_url(&json!({"url": "javascript:x"})).is_err());
    }

    #[tokio::test]
    async fn extracts_album_lazily_with_api_lookups() {
        let (root, server) = serve(vec![
            html(ALBUM_PAGE),
            json(json!({"encrypted": false, "url": "https://c1.bunkr.ru/cat-pic-AbCdEfGh.jpg"})),
            json(json!({"url": "https://c2.bunkr.ru/uuid2-123456789012.mp4"})),
        ]);
        let http = http("Bunkr", root);
        let files = collect(extract(
            &http,
            Url::parse("https://bunkr.cr/a/AID").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["name"], "Cat's pic");
        assert_eq!(k["id"], "AbCdEfGh");
        assert_eq!(k["filename"], "Cat's pic-AbCdEfGh");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["id_url"], "111");
        assert_eq!(k["num"], 1);
        assert_eq!(k["date"], "2024-06-05 12:30:00");
        assert_eq!(
            files[0].headers.get("Referer").map(String::as_str),
            Some("https://get.bunkrr.su/file/111")
        );
        let k = keywords(&files[1]);
        assert_eq!(k["id"], "");
        assert_eq!(k["filename"], "clip");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Cat's pic-AbCdEfGh.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("bunkr/My & Album (AID)")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/AID?advanced=1 "));
        assert!(requests[1].starts_with("POST /api/_001_v2 "));
        assert!(requests[1].ends_with("{\"id\":\"111\"}"));
    }

    #[tokio::test]
    async fn extracts_media_page() {
        let (root, server) = serve(vec![
            html(
                "<h1 class=\"x\">My%20Video.mp4</h1><img src=\"/thumbs/uuidX.png\">\
                 <div data-file-id=\"555\"></div><a href=\"../a/ALB\">",
            ),
            json(json!({"url": "https://c3.bunkr.ru/My-Video-q1w2e3r4.mp4"})),
            html(
                "<meta property=\"og:title\" content=\"Parent\"><span class=\"font-semibold\">(1 GB)</span>",
            ),
        ]);
        let http = http("Bunkr", root);
        let files = collect(extract(
            &http,
            Url::parse("https://bunkr.si/v/My-Video-q1w2e3r4").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "media");
        assert_eq!(k["name"], "My Video");
        assert_eq!(k["slug"], "My-Video-q1w2e3r4");
        assert_eq!(k["uuid"], "uuidX");
        assert_eq!(k["album_id"], "ALB");
        assert_eq!(k["album_name"], "Parent");
        assert_eq!(k["count"], 1);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("bunkr")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v/My-Video-q1w2e3r4 "));
        assert!(requests[2].starts_with("GET /a/ALB "));
    }
}
