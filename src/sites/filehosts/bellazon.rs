//! Bellazon forum posts, threads and forums, mirroring
//! gallery_dl/extractor/bellazon.py: attachments hosted by the forum are
//! downloaded, other links are queued for their extractors. Options:
//! `quoted` (include quoted posts) and `order-posts`.
use crate::error;
use crate::sites::gdl::{
    Scan, date, ext_from_url, extr, extract_iter, jsonld, nameext_from_name, nameext_into,
    parse_int, parse_iso, parse_query, partition, re, split_html, str_of, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.bellazon.com/main";
const FILENAME: &str = "{post[id]}_{num:>02}_{id}_{filename}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{thread[section]}",
    "{thread[title]} ({thread[id]})",
];
const ARCHIVE: &str = "{post[id]}/{id}_{filename}";

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static THREAD: Route = Route::new("thread", FILENAME, DIRECTORY, ARCHIVE);
static FORUM: Route = Route::new("forum", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "bellazon",
    name: "Bellazon",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &THREAD, &FORUM],
    api_key: false,
    cookies: Some("bellazon.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String),
    Thread(String, Option<String>),
    Forum(String, Option<String>),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::gdl::on(url, &["bellazon.com"], true) {
        return None;
    }
    let path = url.path().strip_prefix("/main")?;
    if let Some(fragment) = url.fragment() {
        let comment = fragment
            .strip_prefix("findComment-")
            .or_else(|| fragment.strip_prefix("comment-"));
        if let (Some(id), Some(c)) = (
            comment.filter(|c| crate::sites::gdl::digits(c)),
            re!(r"^(/topic/\d+-[^/?#]+(?:/page/\d+)?)/?$").captures(path),
        ) {
            return Some(Target::Post(c[1].to_owned(), id.to_owned()));
        }
    }
    if let Some(c) = re!(r"^(/topic/\d+-[^/?#]+)(?:/page/(\d+))?").captures(path) {
        return Some(Target::Thread(
            c[1].to_owned(),
            c.get(2).map(|m| m.as_str().to_owned()),
        ));
    }
    let c = re!(r"^(/forum/\d+-[^/?#]+)(?:/page/(\d+))?").captures(path)?;
    Some(Target::Forum(
        c[1].to_owned(),
        c.get(2).map(|m| m.as_str().to_owned()),
    ))
}

/// `_parse_thread`: the thread keywords from JSON-LD and breadcrumbs.
fn parse_thread(page: &str) -> crate::Result<Map<String, Value>> {
    let schema = jsonld(page)?;
    let author = &schema["author"];
    let stats = &schema["interactionStatistic"];
    let url_t = str_of(&schema, "url").to_owned();
    let url_a = str_of(author, "url").to_owned();
    let crumbs = split_html(extr(page, "<nav class=\"ipsBreadcrumb", "</nav>"));
    let path: Vec<String> = crumbs
        .get(2..crumbs.len().saturating_sub(1))
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    let mut thread = Map::new();
    thread.insert("url".into(), url_t.as_str().into());
    thread.insert("title".into(), schema["headline"].clone());
    thread.insert("views".into(), stats[0]["userInteractionCount"].clone());
    thread.insert("posts".into(), stats[1]["userInteractionCount"].clone());
    thread.insert(
        "date".into(),
        date(parse_iso(str_of(&schema, "datePublished"))),
    );
    thread.insert(
        "date_updated".into(),
        date(parse_iso(str_of(&schema, "dateModified"))),
    );
    thread.insert(
        "description".into(),
        unescape(str_of(&schema, "text")).trim().into(),
    );
    thread.insert(
        "section".into(),
        path.len()
            .checked_sub(2)
            .and_then(|i| path.get(i))
            .cloned()
            .unwrap_or_default()
            .into(),
    );
    thread.insert("path".into(), path.into());
    thread.insert("author".into(), author["name"].clone());
    thread.insert("author_url".into(), url_a.as_str().into());
    let (id, _, slug) = partition(url_t.rsplit('/').nth(1).unwrap_or_default(), "-");
    thread.insert("id".into(), id.into());
    thread.insert("slug".into(), unquote(slug).into());
    let (author_id, author_slug) = if url_a.is_empty() {
        (String::new(), String::new())
    } else {
        let (id, _, slug) = partition(url_a.rsplit('/').nth(1).unwrap_or_default(), "-");
        (id.to_owned(), slug.to_owned())
    };
    thread.insert("author_id".into(), author_id.into());
    thread.insert("author_slug".into(), author_slug.into());
    Ok(thread)
}

/// `_parse_post`.
fn parse_post(html: &str) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let id = scan.extr("id=\"elComment_", "\"").to_owned();
    let author_url = scan.extr(" href='", "'").to_owned();
    let posted = date(parse_iso(scan.extr("datetime='", "'")));
    let content = scan.extr("<!-- Post content -->", "<menu data-ips-hook=");
    // content[beg+1:end+1] with Python's -1 results for missing needles.
    let begin = content.find('>').map_or(0, |i| i + 1);
    let limit = match content.rfind("<div data-role=\"memberSignature\"") {
        Some(signature) => signature + 1,
        None => content.len(),
    };
    let content = match content[..limit.min(content.len())].rfind("\n\t\t</div>") {
        Some(end) if end + 1 > begin => content[begin..=end].trim(),
        _ => "",
    };
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    post.insert("author_url".into(), author_url.as_str().into());
    post.insert("date".into(), posted);
    post.insert("content".into(), content.into());
    let (author_id, author_slug) = if author_url.is_empty() {
        (String::new(), String::new())
    } else {
        let (id, _, slug) = partition(author_url.rsplit('/').nth(1).unwrap_or_default(), "-");
        (id.to_owned(), slug.to_owned())
    };
    post.insert("author_id".into(), author_id.into());
    post.insert("author_slug".into(), author_slug.into());
    post
}

/// `_remove_quotes`: drop (nested) blockquotes.
fn remove_quotes(content: &str) -> String {
    let mut content = content.to_owned();
    while let Some(begin) = content.find("<blockquote") {
        let Some(mut end) = content[begin..].find("</blockquote").map(|i| begin + i) else {
            break;
        };
        let nested = content[begin + 11..end].matches("<blockquote").count();
        for _ in 0..nested {
            match content[end + 13..].find("</blockquote") {
                Some(i) => end = end + 13 + i,
                None => break,
            }
        }
        content = format!(
            "{}{}",
            &content[..begin],
            content.get(end + 13..).unwrap_or_default()
        );
    }
    content
}

/// One post's files and queued links.
fn post_events<'a>(
    http: &'a Http,
    route: &'static Route,
    thread: &Map<String, Value>,
    mut post: Map<String, Value>,
) -> Events<'a> {
    let content = post
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let content = if http.option_bool("quoted", false) {
        content
    } else {
        remove_quotes(&content)
    };
    let pattern = re!(
        r#"(?s)<((?:video .*?<source [^>]*?src|a [^>]*?href)="([^"]+).*?</a>|img [^>]*?src="([^"]+)"[^>]*>)"#
    );
    let urls: Vec<(String, String, String)> = pattern
        .captures_iter(&content)
        .map(|c| {
            (
                c[1].to_owned(),
                c.get(2).map_or("", |m| m.as_str()).to_owned(),
                c.get(3).map_or("", |m| m.as_str()).to_owned(),
            )
        })
        .collect();
    post.insert("count".into(), urls.len().into());
    let mut data = Map::new();
    data.insert("thread".into(), Value::Object(thread.clone()));
    data.insert("post".into(), Value::Object(post));
    data.insert("count".into(), urls.len().into());
    let (mut num, mut internal, mut external) = (0, 0, 0);
    let native = [format!("{ROOT}/"), format!("{}/", &ROOT[6..])];
    let mut items = Vec::new();
    let mut queued = Vec::new();
    for (info, url, url_img) in urls {
        let url = if url_img.is_empty() {
            unescape(&url)
        } else {
            unescape(match extr(&info, "data-full-image=\"", "\"") {
                "" => &url_img,
                full => full,
            })
        };
        if native.iter().any(|n| url.starts_with(n.as_str())) {
            if [
                "/main/style_emoticons/",
                "/uploads/emoticons/",
                "/public/style_",
                "/profile/",
                "/topic/",
            ]
            .iter()
            .any(|s| url.contains(s))
            {
                continue;
            }
            num += 1;
            internal += 1;
            let mut dc = data.clone();
            dc.insert("num".into(), num.into());
            dc.insert("num_internal".into(), internal.into());
            dc.insert("num_external".into(), external.into());
            let alt = extr(&info, " alt=\"", "\"");
            if alt.is_empty() || (alt.starts_with("post-") && alt.contains("_thumb.")) {
                nameext_into(&url, &mut dc);
            } else {
                let name = unescape(alt);
                dc.insert("filename".into(), partition(&name, ".").0.into());
                dc.insert("name".into(), name.into());
            }
            dc.insert("id".into(), extr(&info, "data-fileid=\"", "\"").into());
            let ext = extr(&info, "data-fileext=\"", "\"");
            if !ext.is_empty() {
                dc.insert("extension".into(), ext.into());
            } else if url.contains("/core/interface/file/attachment.php")
                || url.contains("/main/index.php?")
            {
                if dc
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(str::is_empty)
                {
                    let params = parse_query(url.split_once('?').map_or("", |(_, q)| q));
                    let id = params
                        .get("id")
                        .or(params.get("attach_id"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    dc.insert("id".into(), id);
                }
                let name = extr(&info, ">", "<").trim();
                if !name.is_empty() {
                    let name = unescape(name);
                    let (stem, ext) = nameext_from_name(&name);
                    dc.insert("filename".into(), stem.into());
                    dc.insert("extension".into(), ext.into());
                    dc.insert("name".into(), name.into());
                }
            } else {
                dc.insert("extension".into(), ext_from_url(&url).into());
            }
            let url = if url.starts_with('/') {
                format!("https:{url}")
            } else {
                url
            };
            if let Ok(file) = Url::parse(&url) {
                items.push(Item::new(file, dc).ids(String::new(), num.to_string()));
            }
        } else {
            num += 1;
            external += 1;
            let mut dc = data.clone();
            dc.insert("num".into(), num.into());
            dc.insert("num_internal".into(), internal.into());
            dc.insert("num_external".into(), external.into());
            if let Ok(child) = Url::parse(&url) {
                queued.push((child, dc));
            }
        }
    }
    let files = crate::sites::events(crate::sites::post(&SITE, http, route, items));
    let children = futures_util::stream::iter(queued)
        .map(move |(url, data)| {
            crate::sites::queue(http, SITE.category, route.subcategory, url, &data)
        })
        .flatten();
    files.chain(children).boxed()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Bellazon URL"));
    };
    match target {
        Target::Post(path, id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
            let pos = page
                .find(&format!("id=\"elComment_{id}"))
                .ok_or_else(|| error("Bellazon post not found"))?;
            let html = Scan::at(&page, pos.saturating_sub(100))
                .extr("<article ", "</article>")
                .to_owned();
            let thread = parse_thread(&page)?;
            Ok(post_events(http, &POST, &thread, parse_post(&html)))
        }),
        Target::Thread(path, page) => {
            let ascending = http
                .option_str("order-posts")
                .and_then(|o| o.chars().next())
                .is_some_and(|c| !matches!(c, 'd' | 'r'));
            let base = format!("{ROOT}{path}");
            crate::sites::once(async move {
                let first_url = if ascending {
                    match &page {
                        None => format!("{base}/"),
                        Some(n) => format!("{base}/page/{n}/"),
                    }
                } else {
                    format!("{base}/page/{}/", page.as_deref().unwrap_or("9999"))
                };
                let (first, final_url) = http.text_final(&http.api(&first_url)?, &[]).await?;
                // Reverse order starts at the last page the redirect lands on.
                let start = if ascending {
                    page.is_none().then_some(1i64)
                } else {
                    let parts: Vec<&str> = final_url.as_str().rsplitn(4, '/').collect();
                    Some(if parts.get(2) == Some(&"page") {
                        parse_int(parts[1])
                    } else {
                        1
                    })
                };
                let thread = parse_thread(&first)?;
                Ok(crate::sites::paginate(Some((first, start)), move |state| {
                    let base = base.clone();
                    let thread = thread.clone();
                    async move {
                        let Some((page, number)) = state else {
                            return Ok(None);
                        };
                        let mut posts: Vec<&str> = extract_iter(&page, "<article ", "</article>");
                        if !ascending {
                            posts.reverse();
                        }
                        let events = futures_util::stream::iter(
                            posts.into_iter().map(parse_post).collect::<Vec<_>>(),
                        )
                        .map({
                            let thread = thread.clone();
                            move |post| post_events(http, &THREAD, &thread, post)
                        })
                        .flatten()
                        .boxed();
                        let next = match (ascending, number) {
                            (true, None) => None,
                            (true, Some(n)) => {
                                if !page.contains(" rel=\"next\" ")
                                    || extr(&page, " rel=\"next\" data-page='", "'")
                                        == n.to_string()
                                {
                                    None
                                } else {
                                    Some(n + 1)
                                }
                            }
                            (false, Some(n)) if n > 1 => Some(n - 1),
                            _ => None,
                        };
                        let next = match next {
                            Some(n) => {
                                let url = if !ascending && n == 1 {
                                    format!("{base}/")
                                } else {
                                    format!("{base}/page/{n}/")
                                };
                                Some((http.text(&http.api(&url)?).await?, Some(n)))
                            }
                            None => None,
                        };
                        Ok(Some((events, next)))
                    }
                }))
            })
        }
        Target::Forum(path, page) => {
            let base = format!("{ROOT}{path}");
            let (first, number) = match &page {
                None => (format!("{base}/"), Some(1i64)),
                Some(n) => (format!("{base}/page/{n}/"), None),
            };
            crate::sites::paginate(Some((first, number)), move |state| {
                let base = base.clone();
                async move {
                    let Some((url, number)) = state else {
                        return Ok(None);
                    };
                    let page = http.text(&http.api(&url)?).await?;
                    let threads: Vec<(Url, Map<String, Value>)> =
                        extract_iter(&page, "<li data-ips-hook=\"topicRow\"", "</")
                            .into_iter()
                            .filter_map(|row| Url::parse(extr(row, "href=\"", "\"")).ok())
                            .map(|u| (u, Map::new()))
                            .collect();
                    let next = match number {
                        Some(n)
                            if page.contains(" rel=\"next\" ")
                                && extr(&page, " rel=\"next\" data-page='", "'")
                                    != n.to_string() =>
                        {
                            Some((format!("{base}/page/{}/", n + 1), Some(n + 1)))
                        }
                        _ => None,
                    };
                    let events = futures_util::stream::iter(threads)
                        .map(move |(url, data)| {
                            crate::sites::queue(http, SITE.category, FORUM.subcategory, url, &data)
                        })
                        .flatten()
                        .boxed();
                    Ok(Some((events, next)))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t(
                "https://www.bellazon.com/main/topic/66334-charly-jordan/page/3/#findComment-4576614"
            ),
            Some(Target::Post(
                "/topic/66334-charly-jordan/page/3".into(),
                "4576614".into()
            ))
        );
        assert_eq!(
            t("https://www.bellazon.com/main/topic/57872-millie-brady/"),
            Some(Target::Thread("/topic/57872-millie-brady".into(), None))
        );
        assert_eq!(
            t("https://www.bellazon.com/main/forum/3-actresses/"),
            Some(Target::Forum("/forum/3-actresses".into(), None))
        );
        assert_eq!(t("https://www.bellazon.com/other"), None);
    }

    #[test]
    fn removes_nested_quotes() {
        assert_eq!(
            remove_quotes("a<blockquote>b<blockquote>c</blockquote>d</blockquote>e"),
            "ae"
        );
    }

    #[tokio::test]
    async fn extracts_post_attachments_and_links() {
        let schema = serde_json::json!({"url": "https://www.bellazon.com/main/topic/57872-millie-brady/", "headline": "Millie Brady",
            "author": {"name": "Ann", "url": "https://www.bellazon.com/main/profile/12-ann/"},
            "interactionStatistic": [{"userInteractionCount": 100}, {"userInteractionCount": 5}],
            "datePublished": "2020-01-01T00:00:00Z", "dateModified": "2020-02-01T00:00:00Z", "text": "Desc"});
        let page = format!(
            r#"<script type="application/ld+json">{schema}</script>
            <nav class="ipsBreadcrumb"><a>Home</a><a>Forums</a><a>Actresses</a><a>Millie Brady</a><a>x</a></nav>
            <article id="x"><div id="elComment_4351049"><a href='https://www.bellazon.com/main/profile/12-ann/'></a><time datetime='2020-01-02T03:04:05Z'>
            <!-- Post content --><div class="c">
            <p><a href="https://www.bellazon.com/main/uploads/monthly_2020/a.jpg" data-fileid="77" data-fileext="jpg"><img src="https://www.bellazon.com/main/uploads/a.thumb.jpg" alt="post-1_thumb.jpg"></a></p>
            <p><a href="https://imx.to/i/abc">external</a></p>
            <blockquote><img src="https://www.bellazon.com/main/uploads/quoted.jpg"></blockquote>
		</div><menu data-ips-hook="x"></article>"#
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = client("bellazon", root);
        let events: Vec<_> = extract(
            &http,
            Url::parse("https://www.bellazon.com/main/topic/57872-millie-brady/#comment-4351049")
                .unwrap(),
        )
        .collect()
        .await;
        let media: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(crate::event::ExtractionEvent::Media(m)) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(media.len(), 1);
        assert_eq!(
            media[0].url.as_str(),
            "https://www.bellazon.com/main/uploads/monthly_2020/a.jpg"
        );
        let k = crate::sites::testing::keywords(media[0]);
        assert_eq!(k["id"], "77");
        assert_eq!(k["thread"]["section"], "Actresses");
        assert_eq!(k["post"]["author_id"], "12");
        assert_eq!(k["post"]["count"], 2);
        let (file, dir) = crate::sites::default_templates(media[0]).unwrap();
        assert_eq!(file.render(media[0]).unwrap(), "4351049_01_77_a.jpg");
        assert_eq!(
            dir.render(media[0]).unwrap().unwrap(),
            std::path::PathBuf::from("bellazon/Actresses/Millie Brady (57872)")
        );
        let children = events
            .iter()
            .filter(|e| matches!(e, Ok(crate::event::ExtractionEvent::BeginChild(_))))
            .count();
        assert_eq!(children, 1);
        server.join().unwrap();
    }
}
