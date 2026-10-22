//! Gelbooru Beta 0.1.11 sites on booru.org (thecollection,
//! illusioncardsbooru, allgirlbooru, drawfriends, vidyart2), mirroring
//! gallery_dl/extractor/gelbooru_v01.py by scraping HTML pages.
use crate::sites::boorus::{
    Extr, booru_fields, date_value, extract_iter, int_value, iso_timestamp, leading_digits,
    unquote_plus, until,
};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

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

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&TAG, &FAVORITE, &POST],
            api_key: false,
        };
    };
}
instance!(THECOLLECTION, "thecollection", "The Collection");
instance!(
    ILLUSIONCARDSBOORU,
    "illusioncardsbooru",
    "Illusion Cards Booru"
);
instance!(ALLGIRLBOORU, "allgirlbooru", "All Girl Booru");
instance!(DRAWFRIENDS, "drawfriends", "Draw Friends");
instance!(VIDYART2, "vidyart2", "/v/idyart2");

fn site(category: &str) -> &'static Site {
    match category {
        "thecollection" => &THECOLLECTION,
        "illusioncardsbooru" => &ILLUSIONCARDSBOORU,
        "allgirlbooru" => &ALLGIRLBOORU,
        "drawfriends" => &DRAWFRIENDS,
        _ => &VIDYART2,
    }
}

fn host(category: &str) -> &'static str {
    match category {
        "thecollection" => "the-collection.booru.org",
        "illusioncardsbooru" => "illusioncards.booru.org",
        "allgirlbooru" => "allgirl.booru.org",
        "drawfriends" => "drawfriends.booru.org",
        _ => "vidyart2.booru.org",
    }
}

fn root(category: &str) -> String {
    format!("https://{}", host(category))
}

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw (still quoted) `tags=` value.
    Tag(String),
    Favorite(String),
    Post(String),
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let rest = crate::sites::boorus::below(url, &[host(category)])?;
    if let Some(tags) = rest.strip_prefix("/index.php?page=post&s=list&tags=") {
        let tags = until(tags, &['&', '#']);
        return (!tags.is_empty()).then(|| Target::Tag(tags.to_owned()));
    }
    if let Some(id) = rest
        .strip_prefix("/index.php?page=favorites&s=view&id=")
        .and_then(leading_digits)
    {
        return Some(Target::Favorite(id.to_owned()));
    }
    rest.strip_prefix("/index.php?page=post&s=view&id=")
        .and_then(leading_digits)
        .map(|id| Target::Post(id.to_owned()))
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error("unsupported Gelbooru v0.1 URL"));
    };
    let base = root(category);
    match target {
        Target::Tag(tags) => {
            let mut data = Map::new();
            data.insert("search_tags".into(), unquote_plus(&tags).into());
            let url = format!("{base}/index.php?page=post&s=list&tags={tags}&pid=");
            pagination(
                category,
                http,
                &TAG,
                url,
                ("class=\"thumb\"><a id=\"p", "\""),
                20,
                data,
            )
        }
        Target::Favorite(id) => {
            let mut data = Map::new();
            data.insert("favorite_id".into(), int_value(&id));
            let url = format!("{base}/index.php?page=favorites&s=view&id={id}&pid=");
            pagination(category, http, &FAVORITE, url, ("posts[", "]"), 50, data)
        }
        Target::Post(id) => crate::sites::once(async move {
            let item = parse_post(category, http, &id, &Map::new()).await?;
            Ok(crate::sites::events(crate::sites::boorus::per_post(
                site(category),
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

/// `_pagination`: post ids from list pages at `url + pid`, then one post
/// page per id.
fn pagination<'a>(
    category: &'static str,
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
                let item = parse_post(category, http, &id, &data).await?;
                let events = crate::sites::boorus::per_post(
                    site(category),
                    http,
                    route,
                    item.into_iter().collect(),
                );
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
/// file URL are skipped like upstream.
async fn parse_post(
    category: &str,
    http: &Http,
    id: &str,
    data: &Map<String, Value>,
) -> Result<Option<Item>> {
    let url = http.api(&format!(
        "{}/index.php?page=post&s=view&id={id}",
        root(category)
    ))?;
    let page = http.text(&url).await?;
    Ok(parse_page(category, id, &page, data))
}

fn parse_page(category: &str, id: &str, page: &str, data: &Map<String, Value>) -> Option<Item> {
    let mut extr = Extr::new(page);
    let mut fields = Map::new();
    let mut put = |key: &str, value: String| {
        fields.insert(key.into(), value.into());
    };
    put("id", id.to_owned());
    let created_at = extr.next("Posted: ", " <").to_owned();
    put("created_at", created_at.clone());
    put("uploader", extr.next("By: ", " <").to_owned());
    put("width", extr.next("Size: ", "x").to_owned());
    put("height", extr.next("", " <").to_owned());
    put("source", extr.next("Source: ", " <").to_owned());
    let rating = extr.next("Rating: ", "<");
    let rating = rating
        .chars()
        .next()
        .unwrap_or('?')
        .to_lowercase()
        .to_string();
    put("rating", rating);
    put("score", extr.next("Score: ", " <").to_owned());
    let file_url = extr.next("<img alt=\"img\" src=\"", "\"").to_owned();
    put("file_url", file_url.clone());
    let tags =
        crate::sites::unescape(extr.next("id=\"tags\" name=\"tags\" cols=\"40\" rows=\"5\">", "<"));
    put("tags", tags.clone());
    let md5 = crate::sites::boorus::before(crate::sites::boorus::rpartition(&file_url, "/").1, ".");
    put("md5", md5.to_owned());
    let timestamp = iso_timestamp(&created_at);
    fields.insert("date".into(), date_value(timestamp));

    if file_url.is_empty() {
        return None;
    }
    let raw = if file_url.starts_with('/') {
        format!("{}{file_url}", root(category))
    } else {
        file_url
    };
    let url = Url::parse(&raw).ok().filter(crate::sites::plain)?;
    booru_fields(&url, &mut fields, data);
    let user = fields
        .get("uploader")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(
        Item::new(url, fields)
            .ids(id, id)
            .user(user)
            .caption(Some(tags))
            .timestamp(timestamp),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    fn t(category: &str, raw: &str) -> Option<Target> {
        target(category, &Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t(
                "allgirlbooru",
                "https://allgirl.booru.org/index.php?page=post&s=list&tags=dress+red&pid=20"
            ),
            Some(Target::Tag("dress+red".into()))
        );
        assert_eq!(
            t(
                "allgirlbooru",
                "https://allgirl.booru.org/index.php?page=post&s=list&tags="
            ),
            None
        );
        assert_eq!(
            t(
                "drawfriends",
                "https://drawfriends.booru.org/index.php?page=favorites&s=view&id=1"
            ),
            Some(Target::Favorite("1".into()))
        );
        assert_eq!(
            t(
                "thecollection",
                "https://the-collection.booru.org/index.php?page=post&s=view&id=100520"
            ),
            Some(Target::Post("100520".into()))
        );
        assert_eq!(
            t(
                "vidyart2",
                "https://www.vidyart2.booru.org/index.php?page=post&s=view&id=1"
            ),
            None
        );
        assert_eq!(
            t(
                "vidyart2",
                "https://vidyart2.booru.org.evil.test/index.php?page=post&s=view&id=1"
            ),
            None
        );
        assert_eq!(
            t(
                "vidyart2",
                "http://vidyart2.booru.org:8080/index.php?page=post&s=view&id=1"
            ),
            None
        );
        assert_eq!(
            t(
                "vidyart2",
                "https://vidyart2.booru.org/index.php?page=pool&s=show&id=1"
            ),
            None
        );
        assert!(
            crate::sites::find(
                &Url::parse("https://illusioncards.booru.org/index.php?page=post&s=view&id=1")
                    .unwrap()
            )
            .is_some_and(|site| site.category == "illusioncardsbooru")
        );
    }

    fn post_page(id: u64) -> String {
        format!(
            r#"<div id="tag_list"><li>Posted: 2017-07-14 02:40:00 <br />By: maker <br />Size: 800x600 <br />Source: https://src.test/x <br />Rating: Safe<br />Score: 5 <span></li></div>
            <img alt="img" src="https://img.booru.org/allgirl//images/1/abc{id}.jpg" width="800">
            <textarea id="tags" name="tags" cols="40" rows="5">dress red &amp;co</textarea>"#
        )
    }

    #[test]
    fn parses_post_page() {
        let item = parse_page("allgirlbooru", "3", &post_page(3), &Map::new()).unwrap();
        let k = &item.keywords;
        assert_eq!(k["id"], "3");
        assert_eq!(k["created_at"], "2017-07-14 02:40:00");
        assert_eq!(k["uploader"], "maker");
        assert_eq!(k["width"], "800");
        assert_eq!(k["height"], "600");
        assert_eq!(k["source"], "https://src.test/x");
        assert_eq!(k["rating"], "s");
        assert_eq!(k["score"], "5");
        assert_eq!(k["tags"], "dress red &co");
        assert_eq!(k["md5"], "abc3");
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["filename"], "abc3");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(item.timestamp, Some(1_500_000_000));
        assert!(parse_page("allgirlbooru", "3", "<html></html>", &Map::new()).is_none());
        let item = parse_page(
            "allgirlbooru",
            "3",
            "Rating: <br /><img alt=\"img\" src=\"/i/a.png\">",
            &Map::new(),
        )
        .unwrap();
        assert_eq!(item.keywords["rating"], "?");
        assert_eq!(item.url.as_str(), "https://allgirl.booru.org/i/a.png");
    }

    #[tokio::test]
    async fn paginates_tag_search() {
        let list: String = (1..=20)
            .map(|i| format!("<span class=\"thumb\"><a id=\"p{i}\" href=\"x\">"))
            .collect();
        let mut replies = vec![html(&list)];
        replies.extend((1..=20).map(|i| html(&post_page(i))));
        replies.push(html("<span class=\"thumb\"><a id=\"p21\">"));
        replies.push(html(&post_page(21)));
        let (root, server) = serve(replies);
        let http = http("allgirlbooru", root);
        let files = media(extract(
            "allgirlbooru",
            &http,
            Url::parse("https://allgirl.booru.org/index.php?page=post&s=list&tags=dress+red%21")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 21);
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "dress red!");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "allgirlbooru_1_abc1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("allgirlbooru/dress red!")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["t_{search_tags}_{id}",["dress red!","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /index.php?page=post&s=list&tags=dress+red%21&pid=0 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /index.php?page=post&s=view&id=1 "));
        assert!(
            requests[21].starts_with("GET /index.php?page=post&s=list&tags=dress+red%21&pid=20 ")
        );
    }

    #[tokio::test]
    async fn favorites_and_post() {
        let (root, server) = serve(vec![
            html("var posts = {}; posts[7] = 1; posts[8] = 2;"),
            html(&post_page(7)),
            html("<html>no image</html>"),
        ]);
        let http = http("drawfriends", root);
        let files = media(extract(
            "drawfriends",
            &http,
            Url::parse("https://drawfriends.booru.org/index.php?page=favorites&s=view&id=12")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["favorite_id"], 12);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("drawfriends/favorites/12")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["f_{favorite_id}_{id}",["12","7"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /index.php?page=favorites&s=view&id=12&pid=0 "));

        let (root, _server) = serve(vec![html(&post_page(5))]);
        let http = crate::sites::testing::http("drawfriends", root);
        let files = media(extract(
            "drawfriends",
            &http,
            Url::parse("https://drawfriends.booru.org/index.php?page=post&s=view&id=5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let meta = files[0].metadata.as_ref().unwrap();
        assert_eq!(meta.username.as_deref(), Some("maker"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "drawfriends_5_abc5.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("drawfriends")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["5"]]"#
        );
    }

    #[tokio::test]
    async fn missing_post_page_is_an_error() {
        let (root, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "text/html",
            b"gone",
        )]);
        let http = http("allgirlbooru", root);
        let events = extract(
            "allgirlbooru",
            &http,
            Url::parse("https://allgirl.booru.org/index.php?page=post&s=view&id=9").unwrap(),
        );
        assert!(crate::sites::testing::fails(events).await);
        server.join().unwrap();
    }
}
