//! Scatbooru tag searches, favorites and posts, mirroring
//! gallery_dl/extractor/scatbooru.py: a Gelbooru Beta 0.1 site whose post
//! pages are scraped one by one from paginated listings.
use crate::sites::boorus::{
    Extr, booru_fields, date_value, extract_iter, int_value, iso_timestamp, leading_digits,
    per_post, unquote_plus, until,
};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const ROOT: &str = "https://scatbooru.co.uk";
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "favorites", "{favorite_id}"],
    "f_{favorite_id}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

pub static SITE: Site = Site {
    category: "scatbooru",
    name: "Scatbooru",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &FAVORITE, &POST],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw (still quoted) `tags=` value.
    Tag(String),
    Favorite(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    let rest = crate::sites::boorus::below(url, &["scatbooru.co.uk", "www.scatbooru.co.uk"])?;
    if let Some(tags) = rest.strip_prefix("/?page=post&s=list&tags=") {
        let tags = until(tags, &['&', '#']);
        return (!tags.is_empty()).then(|| Target::Tag(tags.to_owned()));
    }
    if let Some(id) = rest
        .strip_prefix("/?page=favorites&s=view&id=")
        .and_then(leading_digits)
    {
        return Some(Target::Favorite(id.to_owned()));
    }
    rest.strip_prefix("/?page=post&s=view&id=")
        .and_then(leading_digits)
        .map(|id| Target::Post(id.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Scatbooru URL"));
    };
    match target {
        Target::Tag(tags) => {
            let mut data = Map::new();
            data.insert("search_tags".into(), unquote_plus(&tags).into());
            let url = format!("{ROOT}/?page=post&s=list&tags={tags}&pid=");
            pagination(http, &TAG, url, ("\"><a id=\"p", "\""), 20, data)
        }
        Target::Favorite(id) => {
            let mut data = Map::new();
            data.insert("favorite_id".into(), int_value(&id));
            let url = format!("{ROOT}/?page=favorites&s=view&id={id}&pid=");
            pagination(http, &FAVORITE, url, ("posts[", "]"), 50, data)
        }
        Target::Post(id) => crate::sites::once(async move {
            let item = parse_post(http, &id, &Map::new()).await?;
            Ok(crate::sites::events(per_post(
                &SITE,
                http,
                &POST,
                item.into_iter().collect(),
            )))
        }),
    }
}

struct State {
    ids: VecDeque<String>,
    pid: Option<u64>,
}

/// `GelbooruV01Extractor._pagination`: post IDs from list pages at
/// `url + pid`, then one post page per ID.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    (begin, end): (&'static str, &'static str),
    per_page: u64,
    data: Map<String, Value>,
) -> Events<'a> {
    let state = State {
        ids: VecDeque::new(),
        pid: Some(0),
    };
    crate::sites::paginate(state, move |mut state| {
        let url = url.clone();
        let data = data.clone();
        async move {
            if let Some(id) = state.ids.pop_front() {
                let item = parse_post(http, &id, &data).await?;
                let events = per_post(&SITE, http, route, item.into_iter().collect());
                return Ok(Some((crate::sites::events(events), state)));
            }
            let Some(pid) = state.pid else {
                return Ok(None);
            };
            let page = http.text(&http.api(&format!("{url}{pid}"))?).await?;
            let ids: VecDeque<String> = extract_iter(&page, begin, end)
                .into_iter()
                .map(str::to_owned)
                .collect();
            state.pid = (ids.len() as u64 >= per_page).then_some(pid + per_page);
            state.ids = ids;
            Ok(Some((crate::sites::events(Vec::new()), state)))
        }
    })
}

/// `_parse_post`, then `BooruExtractor.items` bookkeeping. Posts without a
/// file URL are skipped.
async fn parse_post(http: &Http, id: &str, data: &Map<String, Value>) -> Result<Option<Item>> {
    let url = http.api(&format!("{ROOT}/?page=post&s=view&id={id}"))?;
    let page = http.text(&url).await?;
    Ok(parse_page(id, &page, data))
}

fn parse_page(id: &str, page: &str, data: &Map<String, Value>) -> Option<Item> {
    let mut extr = Extr::new(page);
    let mut fields = Map::new();
    let mut put = |key: &str, value: String| {
        fields.insert(key.into(), value.into());
    };
    put("id", id.to_owned());
    put("width", extr.next(">Size:&nbsp;", "x").to_owned());
    put("height", extr.next("", " (").to_owned());
    put("size", extr.next("", ")").to_owned());
    let rating = extr.next(">Rating:&nbsp;", "<");
    let rating = rating
        .chars()
        .next()
        .unwrap_or('?')
        .to_lowercase()
        .to_string();
    put("rating", rating);
    put(
        "source",
        extr.next(">Source:&nbsp;<a href=\"", "\"").to_owned(),
    );
    put("md5", extr.next("&tags=md5%3a", "\"").to_owned());
    put(
        "score",
        crate::sites::boorus::rpartition(extr.next("<a id=\"psc", "<"), ">")
            .1
            .to_owned(),
    );
    let file_url = extr.next("<img alt=\"img\" src=\"", "\"").to_owned();
    put("file_url", file_url.clone());
    let timestamp = iso_timestamp(extr.next(">Posted on ", " by "));
    let uploader = extr.next("\">", "<").to_owned();
    put("uploader", uploader.clone());
    let tags = unescape(extr.next("id=\"tags\" name=\"tags\" cols=\"40\" rows=\"5\">", "<"));
    put("tags", tags.clone());
    fields.insert("date".into(), date_value(timestamp));

    if file_url.is_empty() {
        return None;
    }
    let url = crate::sites::boorus::file_url(ROOT, &file_url)?;
    booru_fields(&url, &mut fields, data);
    Some(
        Item::new(url, fields)
            .ids(id, id)
            .user(Some(uploader))
            .caption(Some(tags))
            .timestamp(timestamp),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("https://scatbooru.co.uk/?page=post&s=list&tags=a+b&pid=20"),
            Some(Target::Tag("a+b".into()))
        );
        assert_eq!(
            t("https://www.scatbooru.co.uk/?page=favorites&s=view&id=1"),
            Some(Target::Favorite("1".into()))
        );
        assert_eq!(
            t("https://scatbooru.co.uk/?page=post&s=view&id=12345"),
            Some(Target::Post("12345".into()))
        );
        for raw in [
            "https://scatbooru.co.uk/?page=post&s=list&tags=",
            "https://scatbooru.co.uk/index.php?page=post&s=view&id=1",
            "https://scatbooru.co.uk.evil.test/?page=post&s=view&id=1",
            "https://scatbooru.co.uk:8080/?page=post&s=view&id=1",
            "https://u:p@scatbooru.co.uk/?page=post&s=view&id=1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post_page(id: u64) -> String {
        format!(
            r#"<li>Size:&nbsp;800x600 (1.2 MB)</li><li>Rating:&nbsp;Safe</li>
<li>Source:&nbsp;<a href="https://src.test/x">x</a></li>
<a href="index.php?page=post&s=list&tags=md5%3aabc{id}">md5</a>
<li>Score: <a id="psc{id}">7</a></li>
<img alt="img" src="https://scatbooru.co.uk/images/1/abc{id}.jpg">
<li>Posted on 2024-05-06 07:08:09 by <a href="/u">maker</a></li>
<textarea id="tags" name="tags" cols="40" rows="5">one two&amp;three</textarea>"#
        )
    }

    #[test]
    fn parses_post_page() {
        let item = parse_page("3", &post_page(3), &Map::new()).unwrap();
        let k = &item.keywords;
        assert_eq!(k["width"], "800");
        assert_eq!(k["height"], "600");
        assert_eq!(k["size"], "1.2 MB");
        assert_eq!(k["rating"], "s");
        assert_eq!(k["source"], "https://src.test/x");
        assert_eq!(k["md5"], "abc3");
        assert_eq!(k["score"], "7");
        assert_eq!(k["uploader"], "maker");
        assert_eq!(k["tags"], "one two&three");
        assert_eq!(k["date"], "2024-05-06 07:08:09");
        assert_eq!(k["filename"], "abc3");
        assert!(parse_page("3", "<html></html>", &Map::new()).is_none());
    }

    #[tokio::test]
    async fn paginates_tag_search() {
        let list: String = (1..=20)
            .map(|i| format!("<span id=\"s{i}\"><a id=\"p{i}\" href=\"x\">"))
            .collect();
        let mut replies = vec![html(&list)];
        replies.extend((1..=20).map(|i| html(&post_page(i))));
        replies.push(html("<span id=\"s21\"><a id=\"p21\">"));
        replies.push(html(&post_page(21)));
        let (root, server) = serve(replies);
        let http = http("scatbooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://scatbooru.co.uk/?page=post&s=list&tags=red+dress%21").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 21);
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "red dress!");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "scatbooru_1_abc1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("scatbooru/red dress!")
        );
        assert_eq!(
            crate::sites::archive_key(&files[20]).unwrap().1,
            r#"["t_{search_tags}_{id}",["red dress!","21"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /?page=post&s=list&tags=red+dress%21&pid=0 "));
        assert!(requests[1].starts_with("GET /?page=post&s=view&id=1 "));
        assert!(requests[21].starts_with("GET /?page=post&s=list&tags=red+dress%21&pid=20 "));
    }

    #[tokio::test]
    async fn favorites_use_fifty_per_page() {
        let (root, server) = serve(vec![
            html("posts[5] = {}; posts[6] = {};"),
            html(&post_page(5)),
            html(&post_page(6)),
        ]);
        let http = http("scatbooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://scatbooru.co.uk/?page=favorites&s=view&id=9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["favorite_id"], 9);
        assert_eq!(
            crate::sites::archive_key(&files[1]).unwrap().1,
            r#"["f_{favorite_id}_{id}",["9","6"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /?page=favorites&s=view&id=9&pid=0 "));
    }
}
