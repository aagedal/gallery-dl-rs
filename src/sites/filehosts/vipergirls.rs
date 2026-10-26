//! ViperGirls threads and posts, mirroring gallery_dl/extractor/vipergirls.py:
//! the `vr.php` XML lists each post's image-host links, which are queued.
//! Options: `domain` (default `viper.click`), `order-posts`, `like`, and
//! `username`/`password` unless `vg_userid`/`vg_password` cookies are loaded.
use crate::error;
use crate::sites::gdl::{auth_info, parse_int};
use crate::sites::{Events, Http, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

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

#[derive(Debug, PartialEq)]
enum Target {
    /// Thread ID and optional `/pageN`.
    Thread(String, Option<String>),
    /// Thread ID and post ID.
    Post(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::gdl::on(url, &["vipergirls.to"], true) {
        return None;
    }
    let path = url.path().strip_prefix("/threads/")?;
    let digits = path.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let thread = path[..digits].to_owned();
    let rest = &path[digits..];
    let rest = match rest.strip_prefix('-') {
        Some(slug) => &slug[slug.find('/').unwrap_or(slug.len())..],
        None => rest,
    };
    let query = url.query();
    if let (Some(query), Some(fragment)) = (query, url.fragment())
        && query.starts_with("p=")
        && query.len() > 2
        && query[2..].starts_with(|c: char| c.is_ascii_digit())
        && let Some(post) = fragment
            .strip_prefix("post")
            .filter(|p| crate::sites::gdl::digits(p))
        && rest.is_empty()
    {
        return Some(Target::Post(thread, post.to_owned()));
    }
    let page = match rest {
        "" => None,
        page if page.starts_with("/page") && crate::sites::gdl::digits(&page[5..]) => {
            Some(page.to_owned())
        }
        _ => return None,
    };
    // `(?:$|#|\?(?!p=))`
    if query.is_some_and(|q| q.starts_with("p=")) {
        return None;
    }
    Some(Target::Thread(thread, page))
}

fn root(http: &Http) -> String {
    match http.option_str("domain").filter(|d| !d.is_empty()) {
        Some(domain) if domain.contains("://") => domain.trim_end_matches('/').to_owned(),
        Some(domain) => format!("https://{}", domain.trim_matches('/')),
        None => "https://viper.click".into(),
    }
}

/// `VipergirlsExtractor.login`.
async fn login(http: &Http, root: &str) -> crate::Result<()> {
    let base = http.api(root)?;
    if http.cookie(&base, "vg_userid").is_some() && http.cookie(&base, "vg_password").is_some() {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let url = http.api(&format!("{root}/login.php?do=login"))?;
    let form = [
        ("vb_login_username", username.as_str()),
        ("vb_login_password", password.as_str()),
        ("do", "login"),
        ("cookieuser", "1"),
    ];
    let _ = http.post_form_text(&url, &form, &[]).await;
    if http.cookie(&base, "vg_password").is_none() {
        return Err(error(
            "ViperGirls login failed; check the username and password",
        ));
    }
    Ok(())
}

fn elements<'a, 'i>(node: roxmltree::Node<'a, 'i>) -> Vec<roxmltree::Node<'a, 'i>> {
    node.children()
        .filter(roxmltree::Node::is_element)
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ViperGirls URL"));
    };
    crate::sites::once(async move {
        let root = root(http);
        login(http, &root).await?;
        let (route, thread_id, page, api) = match &target {
            Target::Thread(thread, page) => (
                &THREAD,
                thread.clone(),
                page.clone(),
                format!("{root}/vr.php?t={thread}"),
            ),
            Target::Post(thread, post) => (
                &POST,
                thread.clone(),
                None,
                format!("{root}/vr.php?p={post}"),
            ),
        };
        let xml = http.text(&http.api(&api)?).await?;
        let doc = roxmltree::Document::parse(&xml)
            .map_err(|_| error("ViperGirls returned invalid XML"))?;
        let top = elements(doc.root_element());
        let attr = |node: Option<&roxmltree::Node<'_, '_>>, name: &str| {
            node.and_then(|n| n.attribute(name))
                .unwrap_or_default()
                .to_owned()
        };
        let forum_title = attr(top.get(1), "title");
        let thread_title = attr(top.get(2), "title");
        let user_hash = attr(top.first(), "hash");
        let like = http.option_bool("like", false) && user_hash.len() >= 16;
        let mut posts: Vec<_> = doc
            .descendants()
            .filter(|n| n.has_tag_name("post"))
            .collect();
        let page_number = page.as_deref().map(|p| parse_int(&p[5..]));
        let ascending = http
            .option_str("order-posts")
            .and_then(|order| order.chars().next())
            .is_some_and(|c| !matches!(c, 'd' | 'r'));
        if ascending {
            if let Some(page) = page_number {
                let skip = usize::try_from((page - 1) * 15).unwrap_or(0);
                posts = posts.into_iter().skip(skip).collect();
            }
        } else {
            if let Some(page) = page_number {
                posts.truncate(usize::try_from(page * 15).unwrap_or(0));
            }
            posts.reverse();
        }
        let mut children: Vec<(Url, Map<String, Value>)> = Vec::new();
        let mut likes = Vec::new();
        for post in posts {
            let images = elements(post);
            let mut data: Map<String, Value> = post
                .attributes()
                .map(|a| (a.name().to_owned(), Value::from(a.value())))
                .collect();
            data.insert("forum_title".into(), forum_title.as_str().into());
            data.insert("thread_id".into(), thread_id.as_str().into());
            data.insert("thread_title".into(), thread_title.as_str().into());
            for (from, to) in [
                ("id", "post_id"),
                ("number", "post_num"),
                ("title", "post_title"),
            ] {
                let value = data.remove(from).unwrap_or(Value::Null);
                data.insert(to.into(), value);
            }
            data.remove("imagecount");
            data.insert("count".into(), images.len().into());
            for (index, image) in images.iter().enumerate() {
                if let Some(url) = image.attribute("main_url").and_then(|u| Url::parse(u).ok()) {
                    let mut data = data.clone();
                    data.insert("num".into(), (index + 1).into());
                    children.push((url, data));
                }
            }
            if like && !images.is_empty() {
                likes.push(post.attribute("id").unwrap_or_default().to_owned());
            }
        }
        for post in likes {
            let mut url = http.api(&format!("{root}/post_thanks.php"))?;
            url.query_pairs_mut()
                .append_pair("do", "post_thanks_add")
                .append_pair("p", &post)
                .append_pair("securitytoken", &user_hash);
            let _ = http.location(&url, &[]).await;
        }
        Ok(futures_util::stream::iter(children)
            .map(move |(url, data)| {
                crate::sites::queue(http, SITE.category, route.subcategory, url, &data)
            })
            .flatten()
            .boxed())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://vipergirls.to/threads/4328304-2011-05-28-Danica"),
            Some(Target::Thread("4328304".into(), None))
        );
        assert_eq!(
            t("https://vipergirls.to/threads/6858916-Karina/page4"),
            Some(Target::Thread("6858916".into(), Some("/page4".into())))
        );
        assert_eq!(
            t("https://vipergirls.to/threads/4328304?foo=bar"),
            Some(Target::Thread("4328304".into(), None))
        );
        assert_eq!(
            t("https://vipergirls.to/threads/4328304-2011?p=116038081&viewfull=1#post116038081"),
            Some(Target::Post("4328304".into(), "116038081".into()))
        );
        assert_eq!(t("https://vipergirls.to/threads/4328304?p=1"), None);
        assert_eq!(t("https://vipergirls.to/forums/1"), None);
    }

    #[tokio::test]
    async fn queues_post_images_newest_last() {
        let xml = r#"<vr><user hash="guest"/><forum title="Forum"/><thread title="Danica"/>
            <post id="2" number="2" title="B" imagecount="1"><image main_url="https://imx.to/i/bbb"/></post>
            <post id="1" number="1" title="A" imagecount="2"><image main_url="https://pixhost.to/show/1/2_a.jpg"/><image main_url="https://example.org/c.jpg"/></post></vr>"#;
        let (root, server) = serve(vec![html(xml)]);
        let http = client("vipergirls", root);
        let queued = children(extract(
            &http,
            Url::parse("https://vipergirls.to/threads/4328304").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 3);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /vr.php?t=4328304 "));
    }
}
