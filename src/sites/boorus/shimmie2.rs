//! Shimmie2 instances (cavemanon, rule34hentai, vidyapics, nozrip,
//! thecollectionS, soybooru), mirroring gallery_dl/extractor/shimmie2.py by
//! scraping the `post/list` and `post/view` HTML pages.
use super::{
    Extr, before, below, leading_digits, parse_bytes, parse_int, quote, rpartition, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{category}_{id}{md5:?_//}.{extension}";
static TAG: Route = Route::new("tag", FILENAME, &["{category}", "{search_tags}"], "{id}");
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&TAG, &POST],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(CAVEMANON, "cavemanon", "Cavemanon");
instance!(RULE34HENTAI, "rule34hentai", "Rule34Hentai");
instance!(VIDYAPICS, "vidyapics", "vidya.pics");
instance!(NOZRIP, "nozrip", "noz.rip");
instance!(THECOLLECTIONS, "thecollectionS", "co.llection.pics");
instance!(SOYBOORU, "soybooru", "soybooru");

/// Per-instance settings from the upstream instance table.
struct Instance {
    site: &'static Site,
    root: &'static str,
    /// The `pattern` entry as `host[/prefix]`.
    pattern: &'static str,
    /// Whether the instance uses the `{0}/index.php?q=image/{2}.{4}` file URL.
    index_file_url: bool,
    /// Fixed quoting character (`quote` instance option).
    quote: Option<&'static str>,
}

fn instance(category: &str) -> Instance {
    let (site, root, pattern, index_file_url, quote): (&'static Site, _, _, _, _) = match category {
        "cavemanon" => (
            &CAVEMANON,
            "https://booru.cavemanon.xyz",
            "booru.cavemanon.xyz",
            true,
            None,
        ),
        "rule34hentai" => (
            &RULE34HENTAI,
            "https://rule34hentai.net",
            "rule34hentai.net",
            false,
            None,
        ),
        "vidyapics" => (&VIDYAPICS, "https://vidya.pics", "vidya.pics", false, None),
        "nozrip" => (
            &NOZRIP,
            "https://noz.rip/booru",
            "noz.rip/booru",
            false,
            None,
        ),
        "thecollectionS" => (
            &THECOLLECTIONS,
            "https://co.llection.pics",
            "co.llection.pics",
            false,
            None,
        ),
        _ => (
            &SOYBOORU,
            "https://soybooru.com",
            "soybooru.com",
            false,
            Some("'"),
        ),
    };
    Instance {
        site,
        root,
        pattern,
        index_file_url,
        quote,
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw tag group and optional page number.
    Tag(String, Option<String>),
    Post(String),
}

/// `BASE_PATTERN + r"post/list/([^/?#]+)(?:/(\d+))?"` and
/// `BASE_PATTERN + r"post/view/(\d+)"`, where `BASE_PATTERN` ends in
/// `/(?:index\.php\?q=/?)?`.
fn target(category: &str, url: &Url) -> Option<Target> {
    let raw = below(url, &[instance(category).pattern])?;
    let rest = raw.strip_prefix('/')?;
    for rest in [
        rest.strip_prefix("index.php?q=")
            .map(|r| r.strip_prefix('/').unwrap_or(r)),
        Some(rest),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(tail) = rest.strip_prefix("post/list/") {
            let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
            if end == 0 {
                continue;
            }
            let page = tail[end..]
                .strip_prefix('/')
                .and_then(leading_digits)
                .map(str::to_owned);
            return Some(Target::Tag(tail[..end].to_owned(), page));
        }
        if let Some(tail) = rest.strip_prefix("post/view/")
            && let Some(id) = leading_digits(tail)
        {
            return Some(Target::Post(id.to_owned()));
        }
    }
    None
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    match target(category, &url) {
        Some(Target::Tag(raw, page)) => tag(category, http, unquote(&raw), page),
        Some(Target::Post(id)) => crate::sites::once(async move {
            let inst = instance(category);
            let mut url = http.api(inst.root)?;
            let prefix = url.path().trim_end_matches('/').to_owned();
            url.set_path(&format!("{prefix}/post/view/{id}"));
            let page = http.text(&url).await?;
            let post = parse_post_page(&inst, &id, &page);
            Ok(crate::sites::events(items(
                &inst,
                http,
                &POST,
                vec![post],
                &Map::new(),
            )))
        }),
        None => crate::sites::fail(error("unsupported Shimmie2 URL")),
    }
}

/// `_quote_type`: the quoting character used in the page.
fn quote_type(inst: &Instance, page: &str) -> String {
    if let Some(quote) = inst.quote {
        return quote.into();
    }
    page.find("<link rel=")
        .and_then(|pos| page[pos + 10..].chars().next())
        .map_or_else(|| "'".into(), String::from)
}

/// Page-format details detected on the first list page.
#[derive(Clone)]
struct Format {
    quote: String,
    has_mime: bool,
    has_pid: bool,
}

struct TagState {
    pnum: i64,
    format: Option<Format>,
}

fn tag<'a>(
    category: &'static str,
    http: &'a Http,
    tags: String,
    page: Option<String>,
) -> Events<'a> {
    let mut data = Map::new();
    data.insert("search_tags".into(), tags.clone().into());
    let pnum = page.as_deref().map_or(1, parse_int);
    crate::sites::paginate(
        Some(TagState { pnum, format: None }),
        move |state: Option<TagState>| {
            let tags = tags.clone();
            let data = data.clone();
            async move {
                let Some(mut state) = state else {
                    return Ok(None);
                };
                let inst = instance(category);
                let mut url = http.api(inst.root)?;
                let prefix = url.path().trim_end_matches('/').to_owned();
                url.set_path(&format!("{prefix}/post/list/{tags}/{}", state.pnum));
                let page = http.text(&url).await?;
                let (posts, next) = parse_list(&inst, &page, &mut state)?;
                let events = items(&inst, http, &TAG, posts, &data);
                Ok(Some((crate::sites::events(events), next.then_some(state))))
            }
        },
    )
}

/// One list page: its posts and whether a next page follows. Advances
/// `state.pnum` like upstream.
fn parse_list(
    inst: &Instance,
    page: &str,
    state: &mut TagState,
) -> Result<(Vec<Map<String, Value>>, bool)> {
    let format = state
        .format
        .get_or_insert_with(|| Format {
            quote: quote_type(inst, page),
            has_mime: page.contains(" data-mime="),
            has_pid: page.contains(" data-post-id="),
        })
        .clone();
    let q = format.quote.as_str();
    let mut extr = Extr::new(page);
    let mut mime = "";
    let mut posts = Vec::new();
    loop {
        if format.has_mime {
            mime = extr.next(&format!(" data-mime={q}"), q);
        }
        let pid = if format.has_pid {
            extr.next(&format!(" data-post-id={q}"), q)
        } else {
            extr.next(" href='/post/view/", q)
        };
        if pid.is_empty() {
            break;
        }
        let title = extr.next(&format!("title={q}"), q);
        let data: Vec<&str> = title.split(" // ").collect();
        let [tags, dimensions, size, ..] = data.as_slice() else {
            return Err(error("unexpected Shimmie2 post list layout"));
        };
        let (width, height) = dimensions.split_once('x').unwrap_or((dimensions, ""));
        let md5 = extr.next("/_thumbs/", "/");
        let ext = if mime.is_empty() {
            "jpg"
        } else {
            rpartition(mime, "/").1
        };
        let file_url = if inst.index_file_url {
            format!("{}/index.php?q=image/{pid}.{ext}", inst.root)
        } else {
            format!(
                "{}/_images/{md5}/{pid}%20-%20{}.{ext}",
                inst.root,
                quote(tags)
            )
        };
        let mut post = Map::new();
        post.insert("file_url".into(), file_url.into());
        post.insert("id".into(), pid.into());
        post.insert("md5".into(), md5.into());
        post.insert("tags".into(), (*tags).into());
        post.insert("width".into(), width.into());
        post.insert("height".into(), height.into());
        let size: String = {
            let mut chars = size.chars();
            chars.next_back();
            chars.collect()
        };
        post.insert("size".into(), parse_bytes(&size).into());
        posts.push(post);
    }
    state.pnum += 1;
    let next = !extr
        .next(&format!("/{}{q}>Next</", state.pnum), ">")
        .is_empty();
    Ok((posts, next))
}

/// `Shimmie2PostExtractor.posts` for a fetched `post/view` page.
fn parse_post_page(inst: &Instance, id: &str, page: &str) -> Map<String, Value> {
    let mut extr = Extr::new(page);
    let qt = quote_type(inst, page);
    let base = match inst.root[8..].find('/') {
        Some(pos) => &inst.root[..pos + 8],
        None => inst.root,
    };
    let tags = before(extr.next(": ", "<"), " - ").trim_end_matches(')');
    let md5 = extr.next("/_thumbs/", "/");
    let mut src = extr.next(&format!("id={qt}main_image{qt} src={qt}"), &qt);
    if src.is_empty() {
        src = extr.next(&format!("<source src={qt}"), &qt);
    }
    let file_url = format!("{base}{}", src.trim_start_matches('.'));
    let width = extr.next("data-width=", " ").trim_matches(['"', '\'']);
    let height = before(extr.next("data-height=", ">"), " ").trim_matches(['"', '\'']);
    let md5 = if md5.is_empty() {
        crate::sites::extr(&file_url, "/_images/", "/").unwrap_or_default()
    } else {
        md5
    }
    .to_owned();
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    post.insert("tags".into(), tags.into());
    post.insert("md5".into(), md5.into());
    post.insert("file_url".into(), file_url.into());
    post.insert("width".into(), width.into());
    post.insert("height".into(), height.into());
    post.insert("size".into(), 0.into());
    post
}

/// `Shimmie2Extractor.items` for a batch of scraped posts.
fn items(
    inst: &Instance,
    http: &Http,
    route: &'static Route,
    posts: Vec<Map<String, Value>>,
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let mut out = Vec::new();
    for post in posts {
        if let Some(item) = finish(post, data) {
            out.push(item);
        }
    }
    super::per_post(inst.site, http, route, out)
}

fn finish(mut post: Map<String, Value>, data: &Map<String, Value>) -> Option<Item> {
    let text = |post: &Map<String, Value>, key: &str| {
        post.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    for key in ["id", "width", "height"] {
        let value = parse_int(&text(&post, key));
        post.insert(key.into(), value.into());
    }
    let tags = unquote(&text(&post, "tags"));
    post.insert("tags".into(), tags.into());
    for (key, value) in data {
        post.insert(key.clone(), value.clone());
    }
    let raw = text(&post, "file_url");
    let url = Url::parse(&raw).ok().filter(crate::sites::plain)?;
    let (filename, extension) = if raw.contains("/index.php?") {
        let (name, ext) = rpartition(rpartition(&raw, "/").1, ".");
        (name.to_owned(), ext.to_owned())
    } else {
        crate::sites::nameext(&url)
    };
    post.insert("filename".into(), filename.into());
    post.insert("extension".into(), extension.into());
    Some(super::item(url, post, "uploader", "tags", None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};
    use futures_util::StreamExt;

    #[test]
    fn classifies_urls() {
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("vidyapics", "https://vidya.pics/post/list/TAG/2"),
            Some(Target::Tag("TAG".into(), Some("2".into())))
        );
        assert_eq!(
            t("vidyapics", "https://vidya.pics/post/list/a%20b"),
            Some(Target::Tag("a%20b".into(), None))
        );
        assert_eq!(
            t(
                "cavemanon",
                "https://booru.cavemanon.xyz/index.php?q=post/list/x/3"
            ),
            Some(Target::Tag("x".into(), Some("3".into())))
        );
        assert_eq!(
            t(
                "cavemanon",
                "https://booru.cavemanon.xyz/index.php?q=/post/view/8"
            ),
            Some(Target::Post("8".into()))
        );
        assert_eq!(
            t("nozrip", "https://noz.rip/booru/post/view/12345"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(
            t("thecollectionS", "https://co.llection.pics/post/view/1"),
            Some(Target::Post("1".into()))
        );
        assert_eq!(t("nozrip", "https://noz.rip/post/view/1"), None);
        assert_eq!(t("nozrip", "https://noz.rip/boorux/post/view/1"), None);
        assert_eq!(t("vidyapics", "https://www.vidya.pics/post/view/1"), None);
        assert_eq!(
            t("vidyapics", "https://vidya.pics.evil.test/post/view/1"),
            None
        );
        assert_eq!(t("vidyapics", "https://vidya.pics:8080/post/view/1"), None);
        assert_eq!(t("vidyapics", "https://vidya.pics/post/view/x"), None);
        assert_eq!(t("vidyapics", "https://vidya.pics/post/list/"), None);
        assert_eq!(t("vidyapics", "https://vidya.pics/index.php?q=x"), None);
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(find("https://soybooru.com/post/view/1"), Some("soybooru"));
        assert_eq!(
            find("https://rule34hentai.net/post/list/a/1"),
            Some("rule34hentai")
        );
    }

    fn list_page(posts: &[(u32, &str)], next: Option<u32>) -> String {
        let mut page =
            String::from("<html><head><link rel='stylesheet' href='/s.css'></head><body>");
        for (id, mime) in posts {
            page.push_str(&format!(
                "<a href='/post/view/{id}' class='thumb shm-thumb' data-tags='t' data-mime='{mime}' data-post-id='{id}'>\
                 <img id='thumb_{id}' title='tag{id} b&amp;c // 640x480 // 1.5MB' src='/_thumbs/md5{id}/thumb.jpg'></a>\n"
            ));
        }
        if let Some(n) = next {
            page.push_str(&format!("<a href='/post/list/x%20y/{n}'>Next</a>"));
        }
        page + "</body></html>"
    }

    #[tokio::test]
    async fn paginates_tag_pages() {
        let (root, server) = serve(vec![
            html(&list_page(&[(1, "image/png"), (2, "video/webm")], Some(3))),
            html(&list_page(&[(3, "image/jpeg")], None)),
        ]);
        let http = http("vidya.pics", root);
        let files = media(extract(
            "vidyapics",
            &http,
            Url::parse("https://vidya.pics/post/list/x%20y/2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 1);
        assert_eq!(k["md5"], "md51");
        assert_eq!(k["tags"], "tag1 b&amp;c");
        assert_eq!(k["width"], 640);
        assert_eq!(k["height"], 480);
        assert_eq!(k["size"], 1_572_864);
        assert_eq!(k["search_tags"], "x y");
        assert_eq!(
            k["file_url"],
            "https://vidya.pics/_images/md51/1%20-%20tag1%20b%26amp%3Bc.png"
        );
        assert_eq!(k["filename"], "1 - tag1 b&amp;c");
        assert_eq!(k["extension"], "png");
        assert_eq!(keywords(&files[1])["extension"], "webm");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "vidyapics_1_md51.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("vidyapics/x y")
        );
        let (category, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(category, "vidyapics");
        assert_eq!(identity, r#"["{id}",["1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /post/list/x%20y/2 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /post/list/x%20y/3 "));
    }

    #[tokio::test]
    async fn cavemanon_uses_index_file_urls() {
        let page = "<link rel=\"stylesheet\"><a href=\"/post/view/7\" data-post-id=\"7\">\
                    <img title=\"a // 10x20 // 2KB\" src=\"/_thumbs/m7/t.jpg\"></a>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("cavemanon", root);
        let files = media(extract(
            "cavemanon",
            &http,
            Url::parse("https://booru.cavemanon.xyz/index.php?q=post/list/a").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            k["file_url"],
            "https://booru.cavemanon.xyz/index.php?q=image/7.jpg"
        );
        assert_eq!(k["filename"], "7");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["size"], 2048);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /post/list/a/1 "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn parses_post_page_below_prefix() {
        let page = "<html><head><title>Post 12: tag1 tag2 - noz.rip</title>\
            <link rel='stylesheet' href='/booru/s.css'>\
            <meta property='og:image' content='https://noz.rip/booru/_thumbs/abc/thumb.jpg'></head>\
            <img alt='main image' id='main_image' src='./booru/_images/abc/12%20-%20tag1.jpg' \
            data-width='640' data-height='480' data-mime='image/jpeg'></html>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("noz.rip", root);
        let files = media(extract(
            "nozrip",
            &http,
            Url::parse("https://noz.rip/booru/post/view/12").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 12);
        assert_eq!(k["tags"], "tag1 tag2");
        assert_eq!(k["md5"], "abc");
        assert_eq!(
            k["file_url"],
            "https://noz.rip/booru/_images/abc/12%20-%20tag1.jpg"
        );
        assert_eq!(k["width"], 640);
        assert_eq!(k["height"], 480);
        assert_eq!(k["size"], 0);
        assert_eq!(k["filename"], "12 - tag1");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "nozrip_12_abc.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("nozrip")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /booru/post/view/12 "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn empty_md5_is_omitted_from_filename() {
        let page = "<title>Post 3: a</title><source src='/media/3.mp4' data-width=\"1\" data-height=\"2\">";
        let (root, _server) = serve(vec![html(page)]);
        let http = http("vidya.pics", root);
        let files = media(extract(
            "vidyapics",
            &http,
            Url::parse("https://vidya.pics/post/view/3").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["md5"], "");
        assert_eq!(k["file_url"], "https://vidya.pics/media/3.mp4");
        assert_eq!(k["width"], 1);
        assert_eq!(k["height"], 2);
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "vidyapics_3.mp4");
    }

    #[tokio::test]
    async fn reports_missing_pages_and_bad_layouts() {
        let (root, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "text/html",
            b"gone",
        )]);
        let site = http("vidya.pics", root);
        let events = extract(
            "vidyapics",
            &site,
            Url::parse("https://vidya.pics/post/view/9").unwrap(),
        );
        let results: Vec<_> = events.collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();

        let page = "<link rel='x'><a href='/post/view/1' data-post-id='1'>\
                    <img title='only tags'></a>";
        let (root, server) = serve(vec![html(page)]);
        let site = http("vidya.pics", root);
        let events = extract(
            "vidyapics",
            &site,
            Url::parse("https://vidya.pics/post/list/a").unwrap(),
        );
        let results: Vec<_> = events.collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
