//! Mobile-app short links of the registry's social sites (`t.snapchat.com`,
//! `v.kuaishou.com`, `xhslink.com`, `v.douyin.com`).
//!
//! Each link is one redirect to a full URL on its site; the target is
//! queued for whichever extractor reads it, keeping its query (such as a
//! Xiaohongshu note's `xsec_token`).
use crate::error;
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

pub static SITE: Site = Site {
    category: "shortlink",
    name: "Short link",
    matches: |url| short(url).is_some(),
    extract,
    routes: &[&LINK],
    api_key: false,
    cookies: None,
};

static LINK: Route = Route::new("link", "{filename}.{extension}", &["{category}"], "");

/// The short-link host and the domain its targets must belong to.
const HOSTS: &[(&str, &str)] = &[
    ("t.snapchat.com", "snapchat.com"),
    ("v.kuaishou.com", "kuaishou.com"),
    ("xhslink.com", "xiaohongshu.com"),
    ("www.xhslink.com", "xiaohongshu.com"),
    ("v.douyin.com", "douyin.com"),
];

fn short(url: &Url) -> Option<&'static str> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path().trim_matches('/');
    let (_, domain) = HOSTS.iter().find(|(short, _)| *short == host)?;
    (!path.is_empty() && path.len() <= 200).then_some(*domain)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(domain) = short(&url) else {
        return crate::sites::fail(error("unsupported short link"));
    };
    crate::sites::once(async move {
        let request = http.api(url.as_str())?;
        let target = http
            .location(&request, &http.browser_headers(&[]))
            .await?
            .map(canonical)
            .filter(|target| {
                target
                    .host_str()
                    .is_some_and(|host| host == domain || host.ends_with(&format!(".{domain}")))
                    && short(target).is_none()
            })
            .ok_or_else(|| error("short link did not redirect to its site"))?;
        Ok(crate::sites::queue(
            http,
            "shortlink",
            LINK.subcategory,
            target,
            &Map::new(),
        ))
    })
}

/// Mobile share hosts that stand for the main site: Kuaishou's
/// `chenzhongtech.com/fw/photo/ID` and Douyin's `iesdouyin.com/share/video/ID`.
fn canonical(target: Url) -> Url {
    let host = target.host_str().unwrap_or_default();
    let segments: Vec<&str> = target.path().split('/').filter(|s| !s.is_empty()).collect();
    let id_ok = |id: &str| {
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
    };
    let rewritten = match (host, segments.as_slice()) {
        (h, ["fw", "photo", id]) if h.ends_with("chenzhongtech.com") && id_ok(id) => {
            Url::parse(&format!("https://www.kuaishou.com/short-video/{id}")).ok()
        }
        (h, ["share", "video" | "note", id]) if h.ends_with("iesdouyin.com") && id_ok(id) => {
            Url::parse(&format!("https://www.douyin.com/video/{id}")).ok()
        }
        _ => None,
    };
    rewritten.unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_short_links_only() {
        let s = |raw: &str| short(&Url::parse(raw).unwrap());
        assert_eq!(s("https://xhslink.com/a/AbC123"), Some("xiaohongshu.com"));
        assert_eq!(s("https://v.kuaishou.com/Kx1"), Some("kuaishou.com"));
        assert_eq!(s("https://go.bsky.app/Q1"), None);
        assert_eq!(s("https://xhslink.com/"), None);
        let c = |raw: &str| canonical(Url::parse(raw).unwrap()).to_string();
        assert_eq!(
            c("https://v.m.chenzhongtech.com/fw/photo/3xhzxsiidvfijsc?cc=share"),
            "https://www.kuaishou.com/short-video/3xhzxsiidvfijsc"
        );
        assert_eq!(
            c("https://www.iesdouyin.com/share/video/7301?did=1"),
            "https://www.douyin.com/video/7301"
        );
        // Unknown targets stay as they are (and are then refused off-site).
        assert_eq!(
            c("https://www.iesdouyin.com/share/user/MS4"),
            "https://www.iesdouyin.com/share/user/MS4"
        );
        assert_eq!(s("https://evil.test/a"), None);
    }

    #[tokio::test]
    async fn queues_the_redirect_target() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: https://www.xiaohongshu.com/discovery/item/abc1?xsec_token=T\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let away = b"HTTP/1.1 302 Found\r\nLocation: https://evil.test/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (root, server) = crate::sites::testing::serve(vec![redirect, away]);
        let http = crate::sites::testing::http("shortlink", root);
        let mut events = extract(&http, Url::parse("https://xhslink.com/a/AbC").unwrap());
        let first = futures_util::StreamExt::next(&mut events)
            .await
            .unwrap()
            .unwrap();
        let crate::event::ExtractionEvent::BeginChild(scope) = first else {
            panic!("expected a queued child");
        };
        scope.control.cancel();
        drop(events);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://xhslink.com/a/AbC").unwrap(),
        ))
        .await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(
            message.contains("did not redirect to its site"),
            "{message}"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/AbC "));
    }
}
