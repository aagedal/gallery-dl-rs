//! Scatbooru tags, favorites and posts, mirroring
//! gallery_dl/extractor/scatbooru.py (a Gelbooru v0.1 site).
use crate::error;
use crate::sites::boorus::{booru_fields, per_post};
use crate::sites::gdl::{
    Scan, captures, date, extract_iter, group, parse_int, parse_iso, re, rpartition, unescape,
    unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
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
    Tag(String),
    Favorite(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?scatbooru\.co\.uk";
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/\?page=post&s=list&tags=([^&#]+)")),
        url,
    ) {
        return Some(Target::Tag(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/\?page=favorites&s=view&id=(\d+)")),
        url,
    ) {
        return Some(Target::Favorite(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/\?page=post&s=view&id=(\d+)")), url)
        .map(|c| Target::Post(group(&c, 1).into()))
}

/// `_parse_post` plus `BooruExtractor.items` bookkeeping.
async fn parse_post(
    http: &Http,
    id: &str,
    data: &Map<String, Value>,
) -> crate::Result<Option<Item>> {
    let page = http
        .text(&http.api(&format!("{ROOT}/?page=post&s=view&id={id}"))?)
        .await?;
    let mut scan = Scan::new(&page);
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    post.insert("width".into(), scan.extr(">Size:&nbsp;", "x").into());
    post.insert("height".into(), scan.extr("", " (").into());
    post.insert("size".into(), scan.extr("", ")").into());
    let rating = match scan.extr(">Rating:&nbsp;", "<") {
        "" => "?".to_owned(),
        rating => rating
            .chars()
            .next()
            .map(|c| c.to_lowercase().to_string())
            .unwrap_or_default(),
    };
    post.insert("rating".into(), rating.into());
    post.insert(
        "source".into(),
        scan.extr(">Source:&nbsp;<a href=\"", "\"").into(),
    );
    post.insert("md5".into(), scan.extr("&tags=md5%3a", "\"").into());
    post.insert(
        "score".into(),
        rpartition(scan.extr("<a id=\"psc", "<"), ">").2.into(),
    );
    let file = scan.extr("<img alt=\"img\" src=\"", "\"").to_owned();
    post.insert("file_url".into(), file.as_str().into());
    post.insert(
        "date".into(),
        date(parse_iso(scan.extr(">Posted on ", " by "))),
    );
    post.insert("uploader".into(), scan.extr("\">", "<").into());
    post.insert(
        "tags".into(),
        unescape(scan.extr("id=\"tags\" name=\"tags\" cols=\"40\" rows=\"5\">", "<")).into(),
    );
    if file.is_empty() {
        return Ok(None);
    }
    let raw = if file.starts_with('/') {
        format!("{ROOT}{file}")
    } else {
        file
    };
    let Some(url) = Url::parse(&raw).ok().filter(crate::sites::plain) else {
        return Ok(None);
    };
    booru_fields(&url, &mut post, data);
    Ok(Some(Item::new(url, post).ids(id, id)))
}

/// `_pagination`: post IDs from list pages, `per_page` IDs per page.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    (begin, end): (&'static str, &'static str),
    per_page: usize,
    data: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(
        (VecDeque::<String>::new(), Some(0usize)),
        move |(mut ids, pid)| {
            let url = url.clone();
            let data = data.clone();
            async move {
                if let Some(id) = ids.pop_front() {
                    let item = parse_post(http, &id, &data).await?;
                    return Ok(Some((
                        crate::sites::events(per_post(
                            &SITE,
                            http,
                            route,
                            item.into_iter().collect(),
                        )),
                        (ids, pid),
                    )));
                }
                let Some(pid) = pid else { return Ok(None) };
                let page = http.text(&http.api(&format!("{url}{pid}"))?).await?;
                let ids: VecDeque<String> = extract_iter(&page, begin, end)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let next = (ids.len() >= per_page).then_some(pid + per_page);
                Ok(Some((crate::sites::events(Vec::new()), (ids, next))))
            }
        },
    )
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Scatbooru URL"));
    };
    match target {
        Target::Tag(tags) => {
            let mut data = Map::new();
            data.insert(
                "search_tags".into(),
                unquote(&tags.replace('+', " ")).into(),
            );
            pagination(
                http,
                &TAG,
                format!("{ROOT}/?page=post&s=list&tags={tags}&pid="),
                ("\"><a id=\"p", "\""),
                20,
                data,
            )
        }
        Target::Favorite(id) => {
            let mut data = Map::new();
            data.insert("favorite_id".into(), parse_int(&id).into());
            pagination(
                http,
                &FAVORITE,
                format!("{ROOT}/?page=favorites&s=view&id={id}&pid="),
                ("posts[", "]"),
                50,
                data,
            )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://scatbooru.co.uk/?page=post&s=view&id=70576"),
            Some(Target::Post("70576".into()))
        );
        assert_eq!(
            t("https://scatbooru.co.uk/?page=post&s=list&tags=4koma"),
            Some(Target::Tag("4koma".into()))
        );
        assert_eq!(
            t("https://scatbooru.co.uk/?page=favorites&s=view&id=1"),
            Some(Target::Favorite("1".into()))
        );
    }

    #[tokio::test]
    async fn extracts_post() {
        let page = r#"<li>Size:&nbsp;1200x800 (300 KB)</li><li>Rating:&nbsp;Explicit</li>
            <li>Source:&nbsp;<a href="https://src.test/1">x</a></li><a href="?page=post&s=list&tags=md5%3aabcdef">md5</a>
            <a id="psc70576">Score: <b>7</b><img alt="img" src="https://scatbooru.co.uk/images/1/abc.jpg">
            <li>Posted on 2020-01-02 03:04:05 by <a href="/u">anon</a></li><textarea id="tags" name="tags" cols="40" rows="5">a b</textarea>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("scatbooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://scatbooru.co.uk/?page=post&s=view&id=70576").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["md5"], "abcdef");
        assert_eq!(k["rating"], "e");
        assert_eq!(k["size"], "300 KB");
        assert_eq!(k["uploader"], "anon");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "scatbooru_70576_abcdef.jpg"
        );
        server.join().unwrap();
    }
}
