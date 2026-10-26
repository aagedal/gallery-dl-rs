//! TumblrGallery blogs, posts and searches, mirroring
//! gallery_dl/extractor/tumblrgallery.py.
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, filename_from_url, group, nameext_from_name, parse_int, re,
    remove_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://tumblrgallery.xyz";
const BASE: &str = r"^https?://tumblrgallery\.xyz";
const FILENAME: &str = "{category}_{gallery_id}_{num:>03}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{gallery_id} {title}"];

static TUMBLRBLOG: Route = Route::new("tumblrblog", FILENAME, DIRECTORY, "{gallery_id}_{num}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{gallery_id}_{num}");
static SEARCH: Route = Route::new(
    "search",
    "{category}_{num:>03}_{gallery_id}_{id}_{title}.{extension}",
    &["{category}", "{search_term}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "tumblrgallery",
    name: "TumblrGallery",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TUMBLRBLOG, &POST, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Blog(String),
    Post(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/tumblrblog/gallery/(\d+)\.html")), url) {
        return Some(Target::Blog(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/post/(\d+)\.html")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/s\.php\?q=([^&#]+)")), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

/// Gallery images with their extra fields.
type Images = Vec<(String, Option<Map<String, Value>>)>;

fn urls_from_page(page: &str) -> Vec<&str> {
    extract_iter(
        page,
        "<div class=\"report\"> <a class=\"xx-co-me\" href=\"",
        "\"",
    )
}

/// `_data_from_url`: the Tumblr id inside the file name.
fn data_from_url(url: &str) -> Map<String, Value> {
    let (filename, _) = nameext_from_name(filename_from_url(url));
    let parts: Vec<&str> = filename.split('_').collect();
    let id = match parts.get(1) {
        Some(&"inline") => parts.get(2).copied(),
        Some(part) => Some(*part),
        None => None,
    };
    let mut data = Map::new();
    data.insert("id".into(), id.unwrap_or(&filename).into());
    data
}

fn post_title(page: &str) -> String {
    remove_html(&unescape(extr(page, "<title>", "</title>"))).replace('_', "-")
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported TumblrGallery URL"));
    };
    crate::sites::once(async move {
        let (route, id, data, images): (&'static Route, String, Map<String, Value>, Images) =
            match target {
                Target::Post(id) => {
                    let page = http
                        .text(&http.api(&format!("{ROOT}/post/{id}.html"))?)
                        .await?;
                    let mut data = Map::new();
                    data.insert("title".into(), post_title(&page).into());
                    data.insert("gallery_id".into(), parse_int(&id).into());
                    let images = urls_from_page(&page)
                        .into_iter()
                        .map(|u| (u.to_owned(), Some(data_from_url(u))))
                        .collect();
                    (&POST, id, data, images)
                }
                Target::Blog(id) => {
                    let page = http
                        .text(&http.api(&format!("{ROOT}/tumblrblog/gallery/{id}.html"))?)
                        .await?;
                    let mut data = Map::new();
                    data.insert(
                        "title".into(),
                        unescape(extr(&page, "<h1>", "</h1>")).into(),
                    );
                    data.insert("gallery_id".into(), parse_int(&id).into());
                    let mut images = Vec::new();
                    for number in 1.. {
                        let request =
                            http.api(&format!("{ROOT}/tumblrblog/gallery/{id}/{number}.html"))?;
                        // Upstream stops at the first redirect or error status.
                        let Ok((page, final_url)) = http.text_final(&request, &[]).await else {
                            break;
                        };
                        if final_url != request {
                            break;
                        }
                        images.extend(
                            urls_from_page(&page)
                                .into_iter()
                                .map(|u| (u.to_owned(), Some(data_from_url(u)))),
                        );
                    }
                    (&TUMBLRBLOG, id, data, images)
                }
                Target::Search(term) => {
                    let mut data = Map::new();
                    data.insert("search_term".into(), term.as_str().into());
                    let mut images = Vec::new();
                    let mut page_url = format!("s.php?q={term}");
                    loop {
                        let page = http.text(&http.api(&format!("{ROOT}/{page_url}"))?).await?;
                        for gallery_id in
                            extract_iter(&page, "<div class=\"title\"><a href=\"post/", ".html")
                        {
                            let post_page = http
                                .text(&http.api(&format!("{ROOT}/post/{gallery_id}.html"))?)
                                .await?;
                            for url in urls_from_page(&post_page) {
                                let mut file = data_from_url(url);
                                file.insert("gallery_id".into(), gallery_id.into());
                                file.insert("title".into(), post_title(&post_page).into());
                                images.push((url.to_owned(), Some(file)));
                            }
                        }
                        let next =
                            extr(&page, "</span> <a class=\"btn btn-primary\" href=\"", "\"");
                        if next.is_empty() || next == page_url {
                            break;
                        }
                        page_url = next.to_owned();
                    }
                    (&SEARCH, term, data, images)
                }
            };
        Ok(crate::sites::events(super::numbered(
            &SITE, http, route, &data, images, &id,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn reads_ids_from_names() {
        assert_eq!(
            data_from_url("https://64.media.tumblr.com/x/tumblr_abc123_1280.jpg")["id"],
            "abc123"
        );
        assert_eq!(
            data_from_url("https://64.media.tumblr.com/x/tumblr_inline_def_500.jpg")["id"],
            "def"
        );
        assert_eq!(data_from_url("https://x/single.jpg")["id"], "single");
    }

    #[tokio::test]
    async fn post_numbers_files() {
        let page = r#"<title>A_B &amp; C</title><div class="report"> <a class="xx-co-me" href="https://64.media.tumblr.com/x/tumblr_abc_1280.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("tumblrgallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://tumblrgallery.xyz/post/405674.html").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "A-B & C");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "tumblrgallery_405674_001_abc.jpg"
        );
        server.join().unwrap();
    }
}
