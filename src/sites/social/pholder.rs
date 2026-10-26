//! Pholder (Reddit media archive) subreddits, users and searches,
//! mirroring gallery_dl/extractor/pholder.py.
use crate::error;
use crate::sites::gdl::{
    Scan, b36encode, captures, date, group, nameext_into, re, split_html, str_of,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Value;
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://pholder.com";
const BASE: &str = r"^https?://(?:www\.)?pholder\.com";
const FILENAME: &str = "{id}{gallery_id:? / /}{title:? //[:225]}.{extension}";
const ARCHIVE: &str = "{id}_{filename}_{gallery_id:? / /}";

static SUBREDDIT: Route = Route::new(
    "subreddit",
    FILENAME,
    &["{category}", "{subredditTitle}"],
    ARCHIVE,
);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{subredditTitle}"],
    ARCHIVE,
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "{subredditTitle}"],
    ARCHIVE,
);

pub static SITE: Site = Site {
    category: "pholder",
    name: "Pholder",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SUBREDDIT, &USER, &SEARCH],
    api_key: false,
    cookies: Some("pholder.com"),
};

fn target(url: &Url) -> Option<(&'static Route, String)> {
    if let Some(c) = captures(re!(&format!(r"{BASE}(/r/([^/?#]+))(?:/?\?([^#]+))?")), url) {
        return Some((&SUBREDDIT, group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}(/u/[^/?#]+)(?:/?\?([^#]+))?")), url) {
        return Some((&USER, group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/(.*)")), url).map(|c| (&SEARCH, group(&c, 1).into()))
}

/// Python `int(text, 0)`.
fn int0(text: &str) -> u64 {
    let text = text.trim();
    let (digits, radix) = match text.get(..2) {
        Some("0x" | "0X") => (&text[2..], 16),
        Some("0o" | "0O") => (&text[2..], 8),
        Some("0b" | "0B") => (&text[2..], 2),
        _ => (text, 10),
    };
    u64::from_str_radix(&digits.replace('_', ""), radix).unwrap_or(0)
}

/// `_solve_challenge`: the `_bcc` cookie value.
fn solve_challenge(html: &str) -> String {
    const MASK: u64 = 0xFFFF_FFFF;
    let mut scan = Scan::new(html);
    let ts = int0(scan.extr("_t=", ","));
    let ip = int0(scan.extr("_b=", ";"));
    let vl = int0(scan.extr(",c=", ";"));
    let mut n = ts ^ ip ^ vl;
    n ^= (n & MASK) >> 16;
    n = (n ^ ((n << 7) & MASK)) & MASK;
    n ^= (n & MASK) >> 3;
    n = (n ^ ((n << 17) & MASK)) & MASK;
    n ^= (n & MASK) >> 11;
    format!("{}.{}", b36encode((ts ^ ip) & MASK), b36encode(n))
}

/// `_parse_window_data`: `window.data`, possibly split across scripts.
fn window_data(html: &str) -> crate::Result<Value> {
    const PREFIX: usize = "window_data = ".len();
    let mut content = String::new();
    let mut split = false;
    for tag in split_html(html) {
        if tag.starts_with("window.data = ") {
            match serde_json::from_str(&tag[PREFIX..]) {
                Ok(value) => return Ok(value),
                Err(_) => split = true,
            }
        }
        if split {
            content.push_str(&tag);
            if let Ok(value) = serde_json::from_str(content.get(PREFIX..).unwrap_or_default()) {
                return Ok(value);
            }
        }
    }
    Err(error("Could not locate window.data JSON."))
}

fn item(entry: &Value) -> Option<Item> {
    let mut data = entry["_source"].as_object()?.clone();
    let id = crate::sites::scalar(&entry["_id"]);
    data.insert(
        "date".into(),
        date(data.get("submitted_utc").and_then(Value::as_i64)),
    );
    let sub = data.remove("sub").unwrap_or_else(|| "".into());
    data.insert("subredditTitle".into(), sub);
    let (id, gallery) = match id.split_once(':') {
        Some((id, gallery)) => (id.to_owned(), gallery.to_owned()),
        None => (id, String::new()),
    };
    data.insert("id".into(), id.as_str().into());
    data.insert("gallery_id".into(), gallery.as_str().into());
    let mut thumbs: Vec<&Value> = data
        .get("thumbnails")
        .and_then(Value::as_array)
        .map(|t| t.iter().collect())
        .unwrap_or_default();
    let area = |t: &Value| -> i64 {
        let n = |key: &str| crate::sites::scalar(&t[key]).parse::<i64>().ok();
        n("width").zip(n("height")).map_or(0, |(w, h)| w * h)
    };
    thumbs.sort_by_key(|t| std::cmp::Reverse(area(t)));
    let url = match thumbs.first() {
        Some(thumb) => {
            let url = str_of(thumb, "url");
            match (url.find(':'), url.rfind(':')) {
                (Some(first), Some(last)) if last > first => url[..last].to_owned(),
                _ => url.to_owned(),
            }
        }
        None => str_of(&Value::Object(data.clone()), "origin").to_owned(),
    };
    nameext_into(&url, &mut data);
    Some(Item::new(Url::parse(&url).ok()?, data).ids(id, gallery))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((route, path)) = target(&url) else {
        return crate::sites::fail(error("unsupported Pholder URL"));
    };
    crate::sites::paginate(Some((1u64, 0u8)), move |state| {
        let path = path.clone();
        async move {
            let Some((page, challenges)) = state else {
                return Ok(None);
            };
            // Upstream `request_interval = (2.0, 4.0)`.
            if !cfg!(test) {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            let mut request = http.api(&format!("{ROOT}/{path}"))?;
            request
                .query_pairs_mut()
                .append_pair("page", &page.to_string());
            let html = http.text(&request).await?;
            if html.len() < 4096 {
                if challenges >= 3 {
                    return Err(error("Pholder challenge was not accepted"));
                }
                let base = http.api(ROOT)?;
                http.set_cookie(
                    &base,
                    &format!(
                        "_bcc={}; Domain=pholder.com; Path=/",
                        solve_challenge(&html)
                    ),
                );
                return Ok(Some((
                    crate::sites::events(Vec::new()),
                    Some((page, challenges + 1)),
                )));
            }
            let data = window_data(&html)?;
            let media = data["media"].as_array().cloned().unwrap_or_default();
            let next = (media.len() >= 150).then_some((page + 1, 0));
            let events = media
                .iter()
                .filter_map(item)
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn solves_challenge_like_upstream() {
        // Python reference: ts=1700000000123, ip=0x7f000001, c=12345.
        let page = "<script>var _t=1700000000123,_b=0x7f000001;x,c=12345;</script>";
        assert_eq!(solve_challenge(page), "1d2ys1m.1id5v6r");
        assert_eq!(b36encode(0), "");
    }

    #[tokio::test]
    async fn reads_window_data() {
        let data = serde_json::json!({"media": [
            {"_id": "abc:2", "_source": {"title": "Cat", "sub": "cats", "submitted_utc": 1700000000,
                "thumbnails": [{"url": "https://i.redd.it/small.jpg", "width": 10, "height": 10}, {"url": "https://pbs.twimg.com/media/big.jpg:large", "width": 100, "height": 100}]}},
            {"_id": "def", "_source": {"title": "", "sub": "cats", "thumbnails": [], "origin": "https://i.imgur.com/x.png"}}]});
        let page = format!("<script>window.data = {data}</script>{}", " ".repeat(4096));
        let (root, server) = serve(vec![html(&page)]);
        let http = client("pholder", root);
        let files = media(extract(
            &http,
            Url::parse("https://pholder.com/r/cats").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://pbs.twimg.com/media/big.jpg");
        assert_eq!(files[1].url.as_str(), "https://i.imgur.com/x.png");
        assert_eq!(keywords(&files[0])["gallery_id"], "2");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "abc 2  Cat.jpg");
        assert_eq!(file.render(&files[1]).unwrap(), "def.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pholder/cats")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET //r/cats?page=1 "));
    }
}
