//! Pholder subreddit, user and search listings, mirroring
//! gallery_dl/extractor/pholder.py: each page's `window.data` JSON lists
//! up to 150 posts, and short pages carry a JavaScript challenge whose
//! answer becomes the `_bcc` cookie.
//!
//! The solved cookies last for the run only (upstream caches them for a
//! day), and upstream's two to four second request interval is not
//! applied.
use crate::sites::art::{json_loads, nameext_into, split_html, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://pholder.com";
const FILENAME: &str = "{id}{gallery_id:? / /}{title:? //[:225]}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{subredditTitle}"];
const ARCHIVE: &str = "{id}_{filename}_{gallery_id:? / /}";
static SUBREDDIT: Route = Route::new("subreddit", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "pholder",
    name: "Pholder",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SUBREDDIT, &USER, &SEARCH],
    api_key: false,
    cookies: Some("pholder.com"),
};

/// The class and the path (with query for searches) upstream requests:
/// `(/r/[^/?#]+)`, `(/u/[^/?#]+)`, else `/(.*)`.
fn target(url: &Url) -> Option<(&'static Route, String)> {
    if !crate::sites::art::on_host(url, &["pholder.com"]) {
        return None;
    }
    let path = url.path();
    for (prefix, route) in [("/r/", &SUBREDDIT), ("/u/", &USER)] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let name = rest.split('/').next().unwrap_or_default();
            if !name.is_empty() {
                return Some((route, format!("{prefix}{name}")));
            }
        }
    }
    let mut rest = path.to_owned();
    if let Some(query) = url.query() {
        rest = format!("{rest}?{query}");
    }
    Some((&SEARCH, rest))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((route, path)) = target(&url) else {
        return crate::sites::fail(error("unsupported Pholder URL"));
    };
    crate::sites::paginate(Some(1u64), move |page| {
        let path = path.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}{path}"))?;
            url.query_pairs_mut().append_pair("page", &page.to_string());
            let mut html = http.text(&url).await?;
            let mut attempts = 0;
            while html.len() < 4096 {
                attempts += 1;
                if attempts > 3 {
                    return Err(error("Pholder challenge was not accepted"));
                }
                let bcc = solve_challenge(&html)
                    .ok_or_else(|| error("Pholder returned an unknown challenge"))?;
                http.set_cookie(&http.api(ROOT)?, &format!("_bcc={bcc}; Path=/"));
                html = http.text(&url).await?;
            }
            let data = window_data(&html)?;
            let media = data
                .get("media")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut events = Vec::new();
            for item in &media {
                if let Some(item) = parse_item(item) {
                    events.extend(crate::sites::post(&SITE, http, route, vec![item]));
                }
            }
            let next = (media.len() >= 150).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// `_parse_window_data`: the `window.data = …` script, which may be split
/// across several script blocks.
pub(crate) fn window_data(html: &str) -> Result<Value> {
    const PREFIX: usize = "window_data = ".len();
    let mut content = String::new();
    let mut split = false;
    for tag in split_html(html) {
        if let Some(json) = tag.strip_prefix("window.data = ") {
            match json_loads(json, "Pholder") {
                Ok(data) => return Ok(data),
                Err(_) => split = true,
            }
        }
        if split {
            content.push_str(&tag);
            if let Some(json) = content.get(PREFIX..)
                && let Ok(data) = json_loads(json, "Pholder")
            {
                return Ok(data);
            }
        }
    }
    Err(error("Could not locate window.data JSON."))
}

/// Python `int(text, 0)`.
fn int0(text: &str) -> Option<u128> {
    let text = text.trim().replace('_', "");
    let lower = text.to_ascii_lowercase();
    let (digits, radix) = if let Some(hex) = lower.strip_prefix("0x") {
        (hex.to_owned(), 16)
    } else if let Some(oct) = lower.strip_prefix("0o") {
        (oct.to_owned(), 8)
    } else if let Some(bin) = lower.strip_prefix("0b") {
        (bin.to_owned(), 2)
    } else {
        (lower, 10)
    };
    u128::from_str_radix(&digits, radix).ok()
}

/// `util.b36encode`.
fn b36(mut num: u128) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while num > 0 {
        out.push(ALPHABET[(num % 36) as usize]);
        num /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `_solve_challenge`: the `_bcc` value for a challenge page.
pub(crate) fn solve_challenge(html: &str) -> Option<String> {
    let mut scan = crate::sites::art::Scan::new(html);
    let ts = int0(scan.extr("_t=", ","))?;
    let ip = int0(scan.extr("_b=", ";"))?;
    let vl = int0(scan.extr(",c=", ";"))?;
    const MASK: u128 = 0xFFFF_FFFF;
    let mut n = ts ^ ip ^ vl;
    n ^= (n & MASK) >> 16;
    n = (n ^ (((n & MASK) << 7) & MASK)) & MASK;
    n ^= (n & MASK) >> 3;
    n = (n ^ (((n & MASK) << 17) & MASK)) & MASK;
    n ^= (n & MASK) >> 11;
    Some(format!("{}.{}", b36((ts ^ ip) & MASK), b36(n)))
}

/// One `media` entry: its largest thumbnail, else the origin URL.
pub(crate) fn parse_item(item: &Value) -> Option<Item> {
    let mut data: Map<String, Value> = item.get("_source")?.as_object()?.clone();
    let id = item
        .get("_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let ts = data
        .get("submitted_utc")
        .and_then(|v| crate::sites::media::ts_value(Some(v)));
    data.insert("date".into(), crate::filter::source_date_json(ts));
    let sub = data.remove("sub").unwrap_or_else(|| "".into());
    data.insert("subredditTitle".into(), sub);
    let (id, gallery) = id.split_once(':').unwrap_or((&id, ""));
    data.insert("id".into(), id.into());
    data.insert("gallery_id".into(), gallery.into());
    let area = |thumb: &Value| -> i64 {
        let dim = |key| crate::sites::scalar(thumb.get(key).unwrap_or(&Value::Null));
        match (dim("width").parse::<i64>(), dim("height").parse::<i64>()) {
            (Ok(w), Ok(h)) => w.saturating_mul(h),
            _ => 0,
        }
    };
    let mut thumbs: Vec<&Value> = data
        .get("thumbnails")
        .and_then(Value::as_array)
        .map(|t| t.iter().collect())
        .unwrap_or_default();
    // Python's stable sort with `reverse=True` keeps equal keys in order.
    thumbs.sort_by_key(|thumb| std::cmp::Reverse(area(thumb)));
    let raw = match thumbs.first() {
        Some(thumb) => {
            let url = str_of(thumb, "url");
            match (url.find(':'), url.rfind(':')) {
                (Some(first), Some(last)) if last > first => url[..last].to_owned(),
                _ => url.to_owned(),
            }
        }
        None => str_of(&Value::Object(data.clone()), "origin").to_owned(),
    };
    let url = crate::sites::media::web_url(&raw)?;
    nameext_into(&raw, &mut data);
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    let author = data
        .get("author")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(
        Item::new(url, data)
            .ids(id, gallery)
            .user(author)
            .caption(title)
            .timestamp(ts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).map(|(r, p)| (r.subcategory, p));
        assert_eq!(
            t("https://pholder.com/r/pics/?x=1"),
            Some(("subreddit", "/r/pics".into()))
        );
        assert_eq!(
            t("https://www.pholder.com/u/bob"),
            Some(("user", "/u/bob".into()))
        );
        assert_eq!(
            t("https://pholder.com/cats?sort=top"),
            Some(("search", "/cats?sort=top".into()))
        );
        for raw in [
            "https://pholder.com.evil/r/pics",
            "https://evilpholder.com/r/pics",
            "https://pholder.com:81/r/pics",
            "https://u@pholder.com/r/pics",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn solves_challenges() {
        let page = "<script>var _t=1700000000,_b=0x7f000001;x,c=12345;</script>";
        let ts: u128 = 1700000000;
        let ip: u128 = 0x7f000001;
        assert_eq!(
            solve_challenge(page).unwrap().split('.').next().unwrap(),
            b36((ts ^ ip) & 0xFFFF_FFFF)
        );
        assert_eq!(b36(35), "z");
        assert_eq!(b36(36), "10");
        assert_eq!(int0("0x10"), Some(16));
    }

    fn page(items: Vec<Value>) -> String {
        let data = json!({"media": items}).to_string();
        let pad = "x".repeat(4096);
        format!("<html><script>window.data = {data}</script><p>{pad}</p></html>")
    }

    #[tokio::test]
    async fn solves_challenge_then_extracts_posts() {
        let challenge = crate::sites::testing::response(
            "200 OK\r\nSet-Cookie: _bcs=srv; Path=/",
            "text/html",
            b"_t=10,_b=3;,c=5;",
        );
        let items = vec![
            json!({"_id": "abc:2", "_source": {"title": "Gallery", "sub": "pics",
                "submitted_utc": 1700000000, "origin": "https://i.redd.it/o.jpg",
                "thumbnails": [
                    {"url": "https://i.imgur.com/s.jpg", "width": 10, "height": 10},
                    {"url": "https://pbs.twimg.com/media/big.jpg:large", "width": 100, "height": "100"}]}}),
            json!({"_id": "def", "_source": {"title": "Single", "origin": "https://i.redd.it/def.png",
                "thumbnails": []}}),
        ];
        let (root, server) = serve(vec![challenge, html(&page(items))]);
        let http = http("pholder", root);
        let files = media(extract(
            &http,
            Url::parse("https://pholder.com/r/pics").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://pbs.twimg.com/media/big.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "abc");
        assert_eq!(k["gallery_id"], "2");
        assert_eq!(k["subredditTitle"], "pics");
        assert_eq!(keywords(&files[1])["subredditTitle"], "");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "abc 2  Gallery.jpg");
        assert_eq!(file.render(&files[1]).unwrap(), "def Single.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pholder/pics")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["{id}_{filename}_{gallery_id:? / /}",["abc","big","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /r/pics?page=1 "));
        assert!(requests[1].starts_with("GET /r/pics?page=1 "));
        let bcc = solve_challenge("_t=10,_b=3;,c=5;").unwrap();
        assert!(
            requests[1].contains(&format!("_bcs=srv; _bcc={bcc}"))
                || requests[1].contains(&format!("_bcc={bcc}; _bcs=srv")),
            "{}",
            requests[1]
        );
    }

    #[test]
    fn joins_split_window_data() {
        let html = "<script>window.data = {\"media\": [</script><script>]}</script>";
        assert_eq!(window_data(html).unwrap(), json!({"media": []}));
        assert!(window_data("<p>none</p>").is_err());
    }
}
