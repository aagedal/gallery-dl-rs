//! Girls with Muscle posts and image searches, mirroring
//! gallery_dl/extractor/girlswithmuscle.py. Searches queue each listed
//! post page by page. Upstream's `username`/`password` login is not
//! supported; pass the site's session cookies with `--cookies`.
use crate::error;
use crate::sites::art::{
    date, extr, extract_iter, leading_digits, on_host, parse_int, parse_iso, split_html,
};
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.girlswithmuscle.com";
const FILENAME: &str = "{model}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{model}"];
const ARCHIVE: &str = "{type}_{model}_{id}";
/// Search pages read at most.
const MAX_PAGES: i64 = 1000;

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "girlswithmuscle",
    name: "Girls with Muscle",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &SEARCH],
    api_key: false,
    cookies: Some("girlswithmuscle.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// Everything after `/images/`, query included.
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["girlswithmuscle.com"]) {
        return None;
    }
    let path = &url.path()[1..];
    if let Some(id) = leading_digits(path) {
        return Some(Target::Post(id.to_owned()));
    }
    let rest = path.strip_prefix("images/")?;
    // `(.*)` runs to the end of the URL but stops at a fragment's newline.
    let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
    let fragment = url.fragment().map(|f| format!("#{f}")).unwrap_or_default();
    Some(Target::Search(format!("{rest}{query}{fragment}")))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(id)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/{id}/"))?).await?;
            if page.is_empty() {
                return Err(error("Girls with Muscle post not found"));
            }
            let items = post(&page, &id).into_iter().collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Some(Target::Search(query)) => search(http, query),
        None => crate::sites::fail(error("unsupported Girls with Muscle URL")),
    }
}

fn post(page: &str, id: &str) -> Option<Item> {
    let (mut data, timestamp) = metadata(page, id);
    let (raw, kind) = match extr(page, "class=\"main-image\" src=\"", "\"") {
        "" => (extr(page, "<source src=\"", "\""), "video"),
        image => (image, "picture"),
    };
    data.insert("type".into(), kind.into());
    crate::sites::art::nameext_into(raw, &mut data);
    let url = crate::sites::art::file_url(raw, ROOT)?;
    let user = data
        .get("uploader")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(
        Item::new(url, data)
            .ids(id, id)
            .user(user)
            .timestamp(timestamp),
    )
}

/// The post's keywords and its upload time.
fn metadata(page: &str, id: &str) -> (Map<String, Value>, Option<i64>) {
    let source = remove_html(extr(
        page,
        "<div id=\"info-source\" style=\"display: none\">",
        "</div>",
    ));
    let image_info = extr(page, "<div class=\"image-info\">", "</div>");
    let uploader = remove_html(extr(image_info, "<span class=\"username-html\">", "</a>"));
    let tags: Vec<String> = split_html(extr(page, "id=\"tags-text\">", "</div>"))
        .into_iter()
        .skip(1)
        .step_by(2)
        .collect();
    let score = parse_int(&remove_html(extr(page, "Score: <b>", "</span")), 0);
    let model = match extr(page, "<title>", "</title>") {
        title if title.starts_with("Picture #") => "unknown",
        title => title,
    };
    let model_list: Vec<String> = if model == "unknown" {
        Vec::new()
    } else {
        model
            .split(',')
            .map(|name| name.trim().to_owned())
            .collect()
    };
    let posted: String = extr(page, "class=\"hover-time\"  title=\"", "\"")
        .chars()
        .take(19)
        .collect();
    let favorite = extr(page, "id=\"favorite-button\">", "</span>");
    let unfavorite = extr(page, "class=\"actionbutton unfavorite-button\">", "</span>");
    let is_favorite = if favorite == "Favorite" {
        Value::Bool(false)
    } else if unfavorite == "Unfavorite" {
        Value::Bool(true)
    } else {
        Value::Null
    };
    let comments: Vec<String> = extract_iter(page, "<div class=\"comment-body-inner\">", "</div>")
        .into_iter()
        .map(|comment| comment.trim().to_owned())
        .collect();
    let mut data = Map::new();
    data.insert("id".into(), id.into());
    data.insert("model".into(), model.into());
    data.insert("model_list".into(), model_list.into());
    data.insert("tags".into(), tags.into());
    let posted = parse_iso(&posted);
    data.insert("date".into(), date(posted));
    data.insert("is_favorite".into(), is_favorite);
    data.insert("source_filename".into(), source.into());
    data.insert("uploader".into(), uploader.into());
    data.insert("score".into(), score.into());
    data.insert("comments".into(), comments.into());
    (data, posted)
}

/// `Page N of M`: the current and last page numbers.
fn page_numbers(page: &str) -> Option<(i64, i64)> {
    let mut rest = page;
    while let Some(found) = rest.find("Page ") {
        rest = &rest[found + 5..];
        let Some(current) = leading_digits(rest) else {
            continue;
        };
        let Some(tail) = rest[current.len()..].strip_prefix(" of ") else {
            continue;
        };
        if let Some(total) = leading_digits(tail) {
            return Some((parse_int(current, 0), parse_int(total, 0)));
        }
    }
    None
}

/// `GirlswithmuscleSearchExtractor`: the first result page, then pages
/// `current + 1 ..= total`, each post queued.
fn search(http: &Http, query: String) -> Events<'_> {
    crate::sites::once(async move {
        let url = http.api(&format!("{ROOT}/images/{query}"))?;
        let (location, page) = http.page(&url, &[]).await?;
        if location != url {
            return Err(error(
                "Girls with Muscle redirected the search; try logging in with --cookies",
            ));
        }
        let (current, total) =
            page_numbers(&page).ok_or_else(|| error("Girls with Muscle search has no pages"))?;
        let last = total.min(current + MAX_PAGES);
        let first = queue_page(http, &page);
        let rest = crate::sites::paginate(current + 1, move |number| {
            let query = query.clone();
            async move {
                if number > last {
                    return Ok(None);
                }
                let url = http.api(&format!("{ROOT}/images/{number}/{query}"))?;
                let page = http.text(&url).await?;
                Ok(Some((queue_page(http, &page), number + 1)))
            }
        });
        Ok(first.chain(rest).boxed())
    })
}

fn queue_page<'a>(http: &'a Http, page: &str) -> Events<'a> {
    let mut data = Map::new();
    data.insert(
        "gallery_name".into(),
        unescape(extr(page, "<title>", "<")).into(),
    );
    let children: Vec<Url> = extract_iter(page, "id=\"imgid-", "\"")
        .into_iter()
        .filter_map(|id| Url::parse(&format!("{ROOT}/{id}/")).ok())
        .filter(|url| matches!(target(url), Some(Target::Post(_))))
        .collect();
    stream::iter(children)
        .map(move |child| {
            crate::sites::queue(http, SITE.category, SEARCH.subcategory, child, &data)
        })
        .flatten()
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.girlswithmuscle.com/12345/"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(
            t("https://girlswithmuscle.com/images/?name=Some%20One"),
            Some(Target::Search("?name=Some%20One".into()))
        );
        for raw in [
            "https://www.girlswithmuscle.com/videos/",
            "https://www.girlswithmuscle.com.evil.test/1/",
            "https://www.girlswithmuscle.com:8443/1/",
            "https://u:p@www.girlswithmuscle.com/1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_picture_post() {
        let page = r#"<title>Jane Doe, Ann Roe</title>
<div id="info-source" style="display: none"><p>orig.jpg</p></div>
<div class="image-info"><span class="username-html"><a>uploader1</a></span></div>
<img class="main-image" src="https://www.girlswithmuscle.com/images/full/12345.jpg">
<div id="tags-text"><b>+</b><a>flexing</a><b>+</b><a>outdoor</a></div>
<span>Score: <b>42</b></span>
<span class="hover-time"  title="2021-03-04 05:06:07.123456+00:00">
<span id="favorite-button">Favorite</span>
<div class="comment-body-inner"> Nice </div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("girlswithmuscle", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.girlswithmuscle.com/12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["model"], "Jane Doe, Ann Roe");
        assert_eq!(k["model_list"], json!(["Jane Doe", "Ann Roe"]));
        assert_eq!(k["tags"], json!(["flexing", "outdoor"]));
        assert_eq!(k["score"], 42);
        assert_eq!(k["date"], "2021-03-04 05:06:07");
        assert_eq!(k["is_favorite"], false);
        assert_eq!(k["source_filename"], "orig.jpg");
        assert_eq!(k["uploader"], "uploader1");
        assert_eq!(k["comments"], json!(["Nice"]));
        assert_eq!(k["type"], "picture");
        assert_eq!(
            path(&files[0]),
            "girlswithmuscle/Jane Doe, Ann Roe/Jane Doe, Ann Roe_12345.jpg"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{type}_{model}_{id}",["picture","Jane Doe, Ann Roe","12345"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /12345/ "));
    }

    #[tokio::test]
    async fn queues_search_pages() {
        let first = "<title>Images of Jane</title>Page 1 of 2<div id=\"imgid-11\"></div><div id=\"imgid-12\"></div>";
        let second = "<title>Images of Jane</title>Page 2 of 2<div id=\"imgid-13\"></div>";
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("girlswithmuscle", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.girlswithmuscle.com/images/?name=Jane").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].subcategory(), "search");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /images/?name=Jane "));
        assert!(requests[1].starts_with("GET /images/2/?name=Jane "));
    }
}
