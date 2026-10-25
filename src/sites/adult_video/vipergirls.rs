//! ViperGirls threads and posts, mirroring gallery_dl/extractor/vipergirls.py
//! through the forum's `vr.php` XML export.
//!
//! Each post is a post boundary whose images are queued to their image
//! hosts; links that no native extractor or the direct-link fallback
//! supports are skipped. Requests go to
//! `https://viper.click` unless the `domain` option names another mirror.
//! Threads list their newest post first unless `order-posts` says
//! otherwise. Upstream's login and `like` option are not supported; pass
//! the forum's `vg_userid`/`vg_password` cookies with `--cookies`.
use crate::sites::art::{leading_digits, on_host, parse_int};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const DEFAULT_ROOT: &str = "https://viper.click";
/// Posts per thread page.
const PER_PAGE: usize = 15;

static THREAD: Route = Route::new("thread", "{filename}.{extension}", &["{category}"], "");
static POST: Route = Route::new("post", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "vipergirls",
    name: "ViperGirls",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&THREAD, &POST],
    api_key: false,
    cookies: Some("vipergirls.to"),
};

/// Cookie domains of the default mirror, besides `vipergirls.to`.
pub(super) const COOKIE_DOMAINS: &[&str] = &["viper.click"];

#[derive(Debug, PartialEq)]
enum Target {
    /// Thread ID and `pageN`.
    Thread(String, Option<String>),
    /// Thread and post ID.
    Post(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["vipergirls.to"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/threads/")?;
    let id = leading_digits(rest)?;
    let mut tail = &rest[id.len()..];
    // `(?:-[^/?#]+)?`
    if let Some(slug) = tail.strip_prefix('-') {
        let end = slug.find('/').unwrap_or(slug.len());
        if end == 0 {
            return None;
        }
        tail = &slug[end..];
    }
    let query = url.query();
    let fragment = url.fragment();
    // `\?p=\d+[^#]*#post(\d+)`
    if tail.is_empty()
        && let Some(post) = query
            .filter(|q| q.strip_prefix("p=").and_then(leading_digits).is_some())
            .and(fragment)
            .and_then(|f| f.strip_prefix("post"))
            .and_then(leading_digits)
    {
        return Some(Target::Post(id.to_owned(), post.to_owned()));
    }
    // `(/page\d+)?(?:$|#|\?(?!p=))`
    let page = match tail.strip_prefix("/page") {
        Some(number) => {
            let digits = leading_digits(number)?;
            if digits.len() != number.len() {
                return None;
            }
            Some(format!("page{digits}"))
        }
        None if tail.is_empty() => None,
        None => return None,
    };
    if query.is_some_and(|q| q.starts_with("p=")) {
        return None;
    }
    Some(Target::Thread(id.to_owned(), page))
}

/// The `domain` option's root, else `https://viper.click`.
fn root(http: &Http) -> String {
    match http.option_str("domain").filter(|d| !d.is_empty()) {
        Some(domain) if domain.contains("://") => domain.trim_end_matches('/').to_owned(),
        Some(domain) => format!("https://{}", domain.trim_matches('/')),
        None => DEFAULT_ROOT.to_owned(),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ViperGirls URL"));
    };
    crate::sites::once(async move {
        let root = root(http);
        let (route, query, thread_id, page) = match target {
            Target::Thread(id, page) => (&THREAD, format!("t={id}"), id, page),
            Target::Post(thread, post) => (&POST, format!("p={post}"), thread, None),
        };
        let xml = http
            .text(&http.api(&format!("{root}/vr.php?{query}"))?)
            .await?;
        let order = http.option_str("order-posts").unwrap_or_default();
        let ascending = !order.is_empty() && !order.starts_with(['d', 'r']);
        let posts = posts(&xml, &thread_id, page.as_deref(), ascending)?;
        let children: Vec<(Url, Map<String, Value>)> = posts
            .into_iter()
            .flat_map(|(data, images)| {
                let count = images.len();
                images
                    .into_iter()
                    .enumerate()
                    .filter_map(move |(index, image)| {
                        let url = Url::parse(&image).ok().filter(crate::sites::plain)?;
                        crate::sites::supported(&url).then_some(())?;
                        let mut data = data.clone();
                        data.insert("count".into(), count.into());
                        data.insert("num".into(), (index + 1).into());
                        Some((url, data))
                    })
            })
            .collect();
        Ok(stream::iter(children)
            .map(move |(child, data)| {
                crate::sites::queue(http, SITE.category, route.subcategory, child, &data)
            })
            .flatten()
            .boxed())
    })
}

/// A post's keywords and image URLs.
type Post = (Map<String, Value>, Vec<String>);

/// Each post's keywords and image URLs, in the requested order: newest
/// first by default, limited to the posts up to `pageN`.
fn posts(xml: &str, thread_id: &str, page: Option<&str>, ascending: bool) -> Result<Vec<Post>> {
    let document = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 262_144,
            ..Default::default()
        },
    )
    .map_err(|_| error("ViperGirls returned invalid XML"))?;
    let root = document.root_element();
    let sections: Vec<_> = root
        .children()
        .filter(roxmltree::Node::is_element)
        .collect();
    let title = |index: usize| {
        sections
            .get(index)
            .and_then(|node| node.attribute("title"))
            .unwrap_or_default()
            .to_owned()
    };
    let (forum_title, thread_title) = (title(1), title(2));
    let mut posts: Vec<_> = root
        .descendants()
        .filter(|node| node.has_tag_name("post"))
        .collect();
    let number = page.map_or(0, |page| parse_int(&page[4..], 0).max(0) as usize);
    if ascending {
        if page.is_some() {
            posts.drain(..((number.max(1) - 1) * PER_PAGE).min(posts.len()));
        }
    } else {
        if page.is_some() {
            posts.truncate(number * PER_PAGE);
        }
        posts.reverse();
    }
    Ok(posts
        .into_iter()
        .map(|post| {
            let images: Vec<String> = post
                .children()
                .filter(roxmltree::Node::is_element)
                .map(|image| image.attribute("main_url").unwrap_or_default().to_owned())
                .collect();
            let mut data: Map<String, Value> = post
                .attributes()
                .map(|attr| (attr.name().to_owned(), attr.value().into()))
                .collect();
            data.insert("forum_title".into(), forum_title.clone().into());
            data.insert("thread_id".into(), thread_id.into());
            data.insert("thread_title".into(), thread_title.clone().into());
            for (from, to) in [
                ("id", "post_id"),
                ("number", "post_num"),
                ("title", "post_title"),
            ] {
                let value = data.remove(from).unwrap_or(Value::Null);
                data.insert(to.into(), value);
            }
            data.insert("count".into(), images.len().into());
            data.remove("imagecount");
            (data, images)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, queued, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://vipergirls.to/threads/4328304-some-title"),
            Some(Target::Thread("4328304".into(), None))
        );
        assert_eq!(
            t("https://www.vipergirls.to/threads/1-x/page3#posts"),
            Some(Target::Thread("1".into(), Some("page3".into())))
        );
        assert_eq!(
            t("https://vipergirls.to/threads/1-x?highlight=a"),
            Some(Target::Thread("1".into(), None))
        );
        assert_eq!(
            t("https://vipergirls.to/threads/1-x?p=23456&viewfull=1#post23456"),
            Some(Target::Post("1".into(), "23456".into()))
        );
        for raw in [
            "https://vipergirls.to/threads/1-x?p=23456",
            "https://vipergirls.to/threads/x",
            "https://vipergirls.to/threads/1-x/other",
            "https://vipergirls.to.evil.test/threads/1",
            "https://vipergirls.to:8080/threads/1",
            "https://u:p@vipergirls.to/threads/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<vr><user hash="guest"/><forum title="Photo Sets" id="1"/><thread title="Garden Set" id="9"/>
<post id="101" number="1" title="First" imagecount="2" username="poster">
<image main_url="https://www.imagebam.com/view/MEABC123" thumb_url="t1"/>
<image main_url="https://unsupported.test/img/1" thumb_url="t2"/>
</post>
<post id="102" number="2" title="" imagecount="1" username="poster">
<image main_url="https://imgbox.com/AbCdEfGh" thumb_url="t3"/>
</post></vr>"#;

    #[test]
    fn orders_posts() {
        let newest = posts(XML, "9", None, false).unwrap();
        assert_eq!(newest[0].0["post_id"], "102");
        let k = &newest[1].0;
        assert_eq!(k["post_num"], "1");
        assert_eq!(k["post_title"], "First");
        assert_eq!(k["forum_title"], "Photo Sets");
        assert_eq!(k["thread_title"], "Garden Set");
        assert_eq!(k["thread_id"], "9");
        assert_eq!(k["count"], 2);
        assert!(!k.contains_key("imagecount") && !k.contains_key("id"));
        let oldest = posts(XML, "9", None, true).unwrap();
        assert_eq!(oldest[0].0["post_id"], "101");
        assert!(posts(XML, "9", Some("page2"), true).unwrap().is_empty());
        assert_eq!(posts(XML, "9", Some("page1"), false).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn queues_supported_images() {
        let (root, server) = serve(vec![response("200 OK", "text/xml", XML.as_bytes())]);
        let http = http("vipergirls", root);
        let children = queued(extract(
            &http,
            Url::parse("https://vipergirls.to/threads/9-garden-set").unwrap(),
        ))
        .await;
        // Only the ImageBam link has a native extractor.
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "thread");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /vr.php?t=9 "));
    }
}
