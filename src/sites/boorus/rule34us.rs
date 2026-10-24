//! rule34.us tag searches and posts, mirroring gallery_dl/extractor/rule34us.py
//! by scraping the HTML pages with `id:<` keyset pagination.
use super::{Extr, below, extract_iter, leading_digits};
use crate::error;
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const ROOT: &str = "https://rule34.us";
const PER_PAGE: usize = 42;
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";

pub static SITE: Site = Site {
    category: "rule34us",
    name: "rule34.us",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &POST],
    api_key: false,
    cookies: None,
};

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw tag group.
    Tag(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    let raw = below(url, &["rule34.us"])?;
    if let Some(tags) = raw.strip_prefix("/index.php?r=posts/index&q=") {
        return Some(Target::Tag(super::until(tags, &['&', '#']).to_owned()));
    }
    raw.strip_prefix("/index.php?r=posts/view&id=")
        .and_then(leading_digits)
        .map(|id| Target::Post(id.to_owned()))
}

/// Which listing page comes next.
enum Next {
    /// `page=page_start`.
    First,
    /// `q=TAGS id:<ID` without `page`.
    Below(String),
    Done,
}

struct State {
    ids: VecDeque<String>,
    next: Next,
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let (route, tags, state) = match target(&url) {
        Some(Target::Tag(raw)) => (
            &TAG,
            Some(super::unquote(&raw.replace('+', " "))),
            State {
                ids: VecDeque::new(),
                next: Next::First,
            },
        ),
        Some(Target::Post(id)) => (
            &POST,
            None,
            State {
                ids: VecDeque::from([id]),
                next: Next::Done,
            },
        ),
        None => return crate::sites::fail(error("unsupported rule34.us URL")),
    };
    let mut data = Map::new();
    if let Some(tags) = &tags {
        data.insert("search_tags".into(), tags.clone().into());
    }
    let tags = tags.unwrap_or_default();
    crate::sites::paginate(state, move |mut state| {
        let data = data.clone();
        let tags = tags.clone();
        async move {
            loop {
                if let Some(id) = state.ids.pop_front() {
                    let mut url = http.api(&format!("{ROOT}/index.php"))?;
                    url.query_pairs_mut()
                        .append_pair("r", "posts/view")
                        .append_pair("id", &id);
                    let page = http.text(&url).await?;
                    let events = match finish(parse_post(&id, &page), &data) {
                        Some(item) => crate::sites::post(&SITE, http, route, vec![item]),
                        None => Vec::new(),
                    };
                    return Ok(Some((crate::sites::events(events), state)));
                }
                let mut url = http.api(&format!("{ROOT}/index.php"))?;
                match &state.next {
                    Next::Done => return Ok(None),
                    Next::First => {
                        url.query_pairs_mut()
                            .append_pair("r", "posts/index")
                            .append_pair("q", &tags)
                            .append_pair("page", "0");
                    }
                    Next::Below(last) => {
                        url.query_pairs_mut()
                            .append_pair("r", "posts/index")
                            .append_pair("q", &format!("{tags} id:<{last}"));
                    }
                }
                let page = http.text(&url).await?;
                let ids = extract_iter(&page, "><a id=\"", "\"");
                state.next = match ids.last() {
                    Some(last) if ids.len() >= PER_PAGE => Next::Below((*last).to_owned()),
                    _ => Next::Done,
                };
                state.ids = ids.into_iter().map(str::to_owned).collect();
            }
        }
    })
}

/// `text.extract(text, begin, end)[0]`, `None` when absent.
fn extract_opt(text: &str, begin: &str, end: &str) -> Value {
    crate::sites::extr(text, begin, end).map_or(Value::Null, Value::from)
}

/// `Rule34usExtractor._parse_post` for a fetched post page.
fn parse_post(id: &str, page: &str) -> Map<String, Value> {
    let mut extr = Extr::new(page);
    let tags = crate::sites::unescape(
        extr.next("name=\"keywords\" content=\"", "\"")
            .trim_end_matches([',', ' ']),
    );
    let uploader = extract_opt(extr.next("Added by: ", "</li>"), ">", "<");
    let score = extract_opt(extr.next("Score: ", "> - <"), ">", "<");
    let width = extr.next("Size: ", "w");
    let height = extr.next(" x ", "h");
    let mut url = extr.next("<source src=\"", "\"");
    if url.is_empty() {
        url = extr.next("<img src=\"", "\"");
    }
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    post.insert("tags".into(), tags.into());
    post.insert("uploader".into(), uploader);
    post.insert("score".into(), score);
    post.insert("width".into(), width.into());
    post.insert("height".into(), height.into());
    // `_fallback` (the original `video-cdn1` URL) is not exposed.
    post.insert(
        "file_url".into(),
        url.replace("//video-cdn1.", "//video.").into(),
    );
    let md5 = super::before(super::rpartition(url, "/").1, ".");
    post.insert("md5".into(), md5.into());
    let mut categories: Vec<(&str, Vec<String>)> = Vec::new();
    for (kind, name) in find_tags(page) {
        let tag = super::unquote(name);
        match categories.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, list)) => list.push(tag),
            None => categories.push((kind, vec![tag])),
        }
    }
    for (kind, list) in categories {
        post.insert(format!("tags_{kind}"), list.join(" ").into());
    }
    post
}

/// `re.findall(r'<li class="([^-"]+)-tag"[^>]*><a href="[^;"]+;q=([^"]+)')`.
fn find_tags(text: &str) -> Vec<(&str, &str)> {
    fn at(text: &str, start: usize) -> Option<(&str, &str, usize)> {
        let class_start = start + "<li class=\"".len();
        let class_end = class_start + text[class_start..].find(['-', '"'])?;
        if class_end == class_start || !text[class_end..].starts_with("-tag\"") {
            return None;
        }
        let rest = class_end + "-tag\"".len();
        let gt = rest + text[rest..].find('>')?;
        let href = gt + "><a href=\"".len();
        if !text[gt..].starts_with("><a href=\"") {
            return None;
        }
        let stop = href + text[href..].find([';', '"'])?;
        if stop == href || !text[stop..].starts_with(";q=") {
            return None;
        }
        let name_start = stop + ";q=".len();
        let name_end = text[name_start..]
            .find('"')
            .map_or(text.len(), |p| name_start + p);
        (name_end > name_start).then(|| {
            (
                &text[class_start..class_end],
                &text[name_start..name_end],
                name_end,
            )
        })
    }
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(offset) = text[pos..].find("<li class=\"") {
        let start = pos + offset;
        match at(text, start) {
            Some((kind, name, end)) => {
                out.push((kind, name));
                pos = end;
            }
            None => pos = start + 1,
        }
    }
    out
}

/// `BooruExtractor.items` for one post.
fn finish(mut post: Map<String, Value>, data: &Map<String, Value>) -> Option<crate::sites::Item> {
    let raw = post.get("file_url")?.as_str()?.to_owned();
    let raw = if raw.starts_with('/') {
        format!("{ROOT}{raw}")
    } else {
        raw
    };
    let url = Url::parse(&raw).ok().filter(crate::sites::plain)?;
    super::booru_fields(&url, &mut post, data);
    Some(super::item(url, post, "uploader", "tags", None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};
    use futures_util::StreamExt;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://rule34.us/index.php?r=posts/index&q=a+b&page=2"),
            Some(Target::Tag("a+b".into()))
        );
        assert_eq!(
            t("https://rule34.us/index.php?r=posts/view&id=12345"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(t("https://rule34.us/index.php?q=a&r=posts/index"), None);
        assert_eq!(t("https://www.rule34.us/index.php?r=posts/view&id=1"), None);
        assert_eq!(
            t("https://rule34.us.evil.test/index.php?r=posts/view&id=1"),
            None
        );
        assert_eq!(
            t("https://rule34.us:8443/index.php?r=posts/view&id=1"),
            None
        );
        assert_eq!(t("https://rule34.us/index.php?r=posts/view&id=x"), None);
    }

    #[test]
    fn finds_tags_like_the_regex() {
        let page = "<li class=\"copyright-tag\" x=\"1\"><a href=\"index.php?r=posts/index&amp;q=a%20b\">\
                    <li class=\"bad-tags\"><a href=\"x;q=n\">\
                    <li class=\"general-tag\"><a href=\"index.php?r=posts/index&amp;q=c\">";
        assert_eq!(
            find_tags(page),
            vec![("copyright", "a%20b"), ("general", "c")]
        );
    }

    fn post_page(id: u32) -> String {
        format!(
            "<meta name=\"keywords\" content=\"a, b&amp;c, \">\
             <li>Added by: <a href=\"/u\">user{id}</a></li>\
             <li>Score: <span>7</span> - <a>up</a></li>\
             <li>Size: 640w x 480h</li>\
             <video><source src=\"https://video-cdn1.rule34.us/images/ab/md5{id}.webm\"></video>\
             <li class=\"general-tag\"><a href=\"index.php?r=posts/index&amp;q=a\">a</a></li>\
             <li class=\"general-tag\"><a href=\"index.php?r=posts/index&amp;q=b%26c\">b</a></li>"
        )
    }

    #[tokio::test]
    async fn paginates_with_id_below() {
        let mut listing = String::new();
        for id in (59..=100).rev() {
            listing.push_str(&format!("<div><a id=\"{id}\" href=\"x\">"));
        }
        let mut replies = vec![html(&listing)];
        replies.extend((59..=100).rev().map(|id| html(&post_page(id))));
        replies.push(html("<div><a id=\"58\" href=\"x\">"));
        replies.push(html(&post_page(58)));
        let (root, server) = serve(replies);
        let http = http("rule34.us", root);
        let files = media(extract(
            &http,
            Url::parse("https://rule34.us/index.php?r=posts/index&q=a+b").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 43);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "100");
        assert_eq!(k["tags"], "a, b&c");
        assert_eq!(k["uploader"], "user100");
        assert_eq!(k["score"], "7");
        assert_eq!(k["width"], "640");
        assert_eq!(k["height"], "480");
        assert_eq!(
            k["file_url"],
            "https://video.rule34.us/images/ab/md5100.webm"
        );
        assert_eq!(k["md5"], "md5100");
        assert_eq!(k["tags_general"], "a b&c");
        assert_eq!(k["extension"], "webm");
        assert_eq!(k["search_tags"], "a b");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "rule34us_100_md5100.webm");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("rule34us/a b")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["a b","100"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /index.php?r=posts%2Findex&q=a+b&page=0 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /index.php?r=posts%2Fview&id=100 "));
        assert!(
            requests[43].starts_with("GET /index.php?r=posts%2Findex&q=a+b+id%3A%3C59 "),
            "{}",
            requests[43]
        );
    }

    #[tokio::test]
    async fn single_post_without_uploader() {
        let page =
            "<meta name=\"keywords\" content=\"x\"><img src=\"https://img.rule34.us/a/abc.png\">";
        let (root, _server) = serve(vec![html(page)]);
        let http = http("rule34.us", root);
        let files = media(extract(
            &http,
            Url::parse("https://rule34.us/index.php?r=posts/view&id=5").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["uploader"], Value::Null);
        assert_eq!(k["score"], Value::Null);
        assert_eq!(k["md5"], "abc");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "rule34us_5_abc.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("rule34us")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["5"]]"#);
    }

    #[tokio::test]
    async fn skips_posts_without_file_and_reports_http_errors() {
        let (root, _server) = serve(vec![
            html("<meta name=\"keywords\" content=\"x\">"),
            response("404 Not Found", "text/html", b""),
        ]);
        let http = http("rule34.us", root);
        let files = media(extract(
            &http,
            Url::parse("https://rule34.us/index.php?r=posts/view&id=5").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let results: Vec<_> = extract(
            &http,
            Url::parse("https://rule34.us/index.php?r=posts/view&id=6").unwrap(),
        )
        .collect()
        .await;
        assert!(results.iter().any(Result::is_err));
    }
}
