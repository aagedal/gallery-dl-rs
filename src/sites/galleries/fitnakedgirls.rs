//! FitNakedGirls galleries, categories, tags, videos and blog posts,
//! mirroring gallery_dl/extractor/fitnakedgirls.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_int, parse_iso, re,
    rpartition, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://fitnakedgirls.com";
const BASE: &str = r"^https?://(?:www\.)?fitnakedgirls\.com";
const DIRECTORY: &[&str] = &["{category}", "{title}"];

static GALLERY: Route = Route::new(
    "gallery",
    "{filename}.{extension}",
    DIRECTORY,
    "{gallery_id}_{filename}",
);
static CATEGORY: Route = Route::new("category", "{filename}.{extension}", &["{category}"], "");
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static VIDEO: Route = Route::new(
    "video",
    "{filename}.{extension}",
    DIRECTORY,
    "{video_id}_{filename}",
);
static BLOG: Route = Route::new(
    "blog",
    "{filename}.{extension}",
    DIRECTORY,
    "{post_id}_{filename}",
);

pub static SITE: Site = Site {
    category: "fitnakedgirls",
    name: "FitNakedGirls",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &CATEGORY, &TAG, &VIDEO, &BLOG],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Category(String),
    Tag(String),
    Video(String, String, String),
    Blog(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/photos/gallery/category/([\w-]+)")),
        url,
    ) {
        return Some(Target::Category(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/photos/gallery/tag/([\w-]+)")), url) {
        return Some(Target::Tag(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/photos/gallery/([\w-]+)/?$")), url) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/videos/(\d+)/(\d+)/([\w-]+)")), url) {
        return Some(Target::Video(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
        ));
    }
    captures(re!(&format!(r"{BASE}/fitblog/([\w-]+)")), url)
        .map(|c| Target::Blog(group(&c, 1).into()))
}

/// `_extract_title`.
fn title(scan: &mut Scan, sep: &str) -> String {
    let title = unescape(scan.extr("<title>", "<"));
    let title = if title.contains(sep) {
        rpartition(&title, sep).0.to_owned()
    } else {
        title
    };
    title.trim().to_owned()
}

/// Page header fields shared by galleries, videos and blog posts.
fn header(page: &str, id_key: &str, sep: &str) -> (String, Map<String, Value>) {
    let mut scan = Scan::new(page);
    let title = title(&mut scan, sep);
    let mut data = Map::new();
    data.insert(
        id_key.into(),
        parse_int(scan.extr("data-post-id=\"", "\"")).into(),
    );
    data.insert(
        "date".into(),
        date(parse_iso(
            scan.extr("article:published_time\" content=\"", "\""),
        )),
    );
    (title, data)
}

fn content(page: &str) -> &str {
    Some(extr(
        page,
        "itemprop=\"articleBody\"",
        "<!-- .entry-content -->",
    ))
    .filter(|c| !c.is_empty())
    .unwrap_or(page)
}

fn uploads(src: &str) -> Option<String> {
    (!src.is_empty() && src.contains("/wp-content/uploads/")).then(|| src.to_owned())
}

/// `FitnakedgirlsGalleryExtractor.images`.
fn gallery_images(page: &str) -> Vec<String> {
    let content = content(page);
    let mut results: Vec<String> =
        extract_iter(content, "<figure class=\"wp-block-video\">", "</figure>")
            .into_iter()
            .filter_map(|f| uploads(extr(f, "src=\"", "\"")))
            .collect();
    results.extend(
        extract_iter(content, "<figure class=\"wp-block-image", "</figure>")
            .into_iter()
            .filter_map(|f| uploads(extr(f, "data-src=\"", "\""))),
    );
    if results.is_empty() {
        results = extract_iter(content, "<img ", ">")
            .into_iter()
            .filter(|img| img.contains("size-large"))
            .filter_map(|img| uploads(extr(img, "data-src=\"", "\"")))
            .collect();
    }
    results
}

fn files<'a>(
    route: &'static Route,
    http: &'a Http,
    data: &Map<String, Value>,
    id: &str,
    urls: Vec<String>,
) -> Events<'a> {
    let items = urls
        .into_iter()
        .filter_map(|src| {
            let mut fields = data.clone();
            nameext_into(&src, &mut fields);
            let name = crate::sites::scalar(&fields["filename"]);
            Some(Item::new(Url::parse(&src).ok()?, fields).ids(id, name))
        })
        .collect();
    crate::sites::events(crate::sites::post(&SITE, http, route, items))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported FitNakedGirls URL"));
    };
    match target {
        Target::Category(name) => listing(
            http,
            &CATEGORY,
            format!("{ROOT}/photos/gallery/category/{name}/"),
        ),
        Target::Tag(name) => listing(http, &TAG, format!("{ROOT}/photos/gallery/tag/{name}/")),
        Target::Gallery(slug) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/photos/gallery/{slug}/"))?)
                .await?;
            let (mut title, mut data) = header(&page, "gallery_id", " - ");
            if let Some(pattern) = [" Nudes", " Nude", " nudes", " nude"]
                .into_iter()
                .find(|p| title.contains(p))
            {
                title = title.split(pattern).next().unwrap_or_default().to_owned();
            }
            data.insert("gallery_slug".into(), slug.into());
            data.insert("model".into(), title.as_str().into());
            data.insert("title".into(), title.into());
            let id = crate::sites::scalar(&data["gallery_id"]);
            let images = gallery_images(&page)
                .into_iter()
                .map(|u| (u, None))
                .collect();
            Ok(crate::sites::events(super::numbered(
                &SITE, http, &GALLERY, &data, images, &id,
            )))
        }),
        Target::Video(year, month, slug) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/videos/{year}/{month}/{slug}/"))?)
                .await?;
            let (title, mut data) = header(&page, "video_id", " | ");
            data.insert("slug".into(), slug.into());
            data.insert("title".into(), title.into());
            let id = crate::sites::scalar(&data["video_id"]);
            let urls = extract_iter(content(&page), "<video ", "</video>")
                .into_iter()
                .filter_map(|v| uploads(extr(v, "src=\"", "\"")))
                .collect();
            Ok(files(&VIDEO, http, &data, &id, urls))
        }),
        Target::Blog(slug) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/fitblog/{slug}/"))?)
                .await?;
            let (title, mut data) = header(&page, "post_id", " - ");
            data.insert("slug".into(), slug.into());
            data.insert("title".into(), title.into());
            let id = crate::sites::scalar(&data["post_id"]);
            let urls = extract_iter(
                content(&page),
                "<figure class=\"wp-block-image",
                "</figure>",
            )
            .into_iter()
            .filter_map(|figure| {
                let srcset = extr(figure, "srcset=\"", "\"");
                if !srcset.is_empty() {
                    let src = srcset
                        .split(", ")
                        .last()
                        .unwrap_or_default()
                        .split(' ')
                        .next()
                        .unwrap_or_default();
                    if let Some(src) = uploads(src) {
                        return Some(src);
                    }
                }
                uploads(extr(figure, "src=\"", "\""))
            })
            .collect();
            Ok(files(&BLOG, http, &data, &id, urls))
        }),
    }
}

/// `_pagination`: gallery links until the next page is not linked.
fn listing<'a>(http: &'a Http, route: &'static Route, base: String) -> Events<'a> {
    crate::sites::paginate(Some((base.clone(), 1u64)), move |state| {
        let base = base.clone();
        async move {
            let Some((url, number)) = state else {
                return Ok(None);
            };
            let page = http.text(&http.api(&url)?).await?;
            let children = extract_iter(&page, "class=\"entry-body", "</a>")
                .into_iter()
                .filter_map(|post| {
                    Some((Url::parse(extr(post, "href=\"", "\"")).ok()?, Map::new()))
                })
                .collect();
            let next = format!("{base}page/{}/", number + 1);
            let next = page
                .contains(&format!("href=\"{next}\""))
                .then_some((next, number + 1));
            Ok(Some((super::queue(http, &SITE, route, children), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fitnakedgirls.com/photos/gallery/jane-nude/"),
            Some(Target::Gallery("jane-nude".into()))
        );
        assert_eq!(
            t("https://fitnakedgirls.com/photos/gallery/category/fit/"),
            Some(Target::Category("fit".into()))
        );
        assert_eq!(
            t("https://fitnakedgirls.com/photos/gallery/tag/abs/"),
            Some(Target::Tag("abs".into()))
        );
        assert_eq!(
            t("https://fitnakedgirls.com/videos/2025/08/clip/"),
            Some(Target::Video("2025".into(), "08".into(), "clip".into()))
        );
        assert_eq!(
            t("https://fitnakedgirls.com/fitblog/jane/"),
            Some(Target::Blog("jane".into()))
        );
    }

    #[tokio::test]
    async fn gallery_lists_videos_then_images() {
        let page = r#"<title>Jane Doe Nudes - FitNakedGirls</title><article data-post-id="77"><meta property="article:published_time" content="2024-01-02T03:04:05+00:00">
            <div itemprop="articleBody"><figure class="wp-block-image"><img data-src="https://fitnakedgirls.com/wp-content/uploads/a.jpg"></figure>
            <figure class="wp-block-video"><video src="https://fitnakedgirls.com/wp-content/uploads/b.mp4"></video></figure><!-- .entry-content -->"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("fitnakedgirls", root);
        let files = media(extract(
            &http,
            Url::parse("https://fitnakedgirls.com/photos/gallery/jane-nude/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert!(files[0].url.as_str().ends_with("b.mp4"));
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Jane Doe");
        assert_eq!(k["gallery_id"], 77);
        assert_eq!(k["num"], 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn category_follows_linked_pages() {
        let first = r#"<div class="entry-body"><a href="https://fitnakedgirls.com/photos/gallery/a/">A</a> <a href="https://fitnakedgirls.com/photos/gallery/category/fit/page/2/">"#;
        let second = r#"<div class="entry-body"><a href="https://fitnakedgirls.com/photos/gallery/b/">B</a>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("fitnakedgirls", root);
        let queued = children(extract(
            &http,
            Url::parse("https://fitnakedgirls.com/photos/gallery/category/fit/").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /photos/gallery/category/fit/page/2/ "));
    }
}
