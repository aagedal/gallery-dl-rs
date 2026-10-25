//! General-purpose URL shorteners, mirroring
//! gallery_dl/extractor/urlshortener.py: the short link's `Location` is
//! queued for whichever extractor handles it.
use crate::error;
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

static LINK: Route = Route::new("link", "{filename}.{extension}", &["{category}"], "");

/// One `UrlshortenerExtractor` instance: its category, host and root.
macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $host:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(url, $host).is_some(),
            extract: |http, url| extract(&$ident, $host, http, url),
            routes: &[&LINK],
            api_key: false,
            cookies: None,
        };
    };
}

instance!(BITLY, "bitly", "Bitly", "bit.ly");
instance!(TCO, "tco", "t.co", "t.co");

/// `BASE_PATTERN + r"(/[^/?#]+)"` on the instance's host.
fn target(url: &Url, host: &str) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some(host) {
        return None;
    }
    let segment = url.path().strip_prefix('/')?.split('/').next()?;
    (!segment.is_empty() && segment.len() <= 200).then(|| segment.to_owned())
}

fn extract<'a>(site: &'static Site, host: &str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(segment) = target(&url, host) else {
        return crate::sites::fail(error("unsupported short link"));
    };
    let root = format!("https://{host}/{segment}");
    crate::sites::once(async move {
        let request = http.api(&root)?;
        // t.co answers browsers with a 200 refresh page instead of a
        // redirect, so it keeps the plain default User-Agent.
        let headers = if site.category == "tco" {
            Vec::new()
        } else {
            http.browser_headers(&[])
        };
        let location = http
            .location(&request, &headers)
            .await?
            .filter(crate::sites::plain)
            .ok_or_else(|| error("Unable to resolve short URL"))?;
        Ok(crate::sites::queue(
            http,
            site.category,
            LINK.subcategory,
            location,
            &Map::new(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |site: &Site, raw: &str| (site.matches)(&Url::parse(raw).unwrap());
        assert!(t(&BITLY, "https://bit.ly/abcde"));
        assert!(t(&BITLY, "http://bit.ly/abcde?x=1"));
        assert!(t(&TCO, "https://t.co/abc123"));
        for raw in [
            "https://bit.ly/",
            "https://www.bit.ly/abc",
            "https://bit.ly.evil.test/abc",
            "https://bit.ly:8443/abc",
            "https://u:p@bit.ly/abc",
        ] {
            assert!(!t(&BITLY, raw), "{raw}");
        }
        assert!(!t(&TCO, "https://bit.ly/abc"));
        assert_eq!(
            crate::sites::find(&Url::parse("https://t.co/x").unwrap()).map(|s| s.category),
            Some("tco")
        );
    }

    fn moved(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn queues_the_location() {
        let (root, server) = serve(vec![moved("https://speakerdeck.com/u/deck")]);
        let http = http("t.co", root);
        let children = queued(extract(
            &TCO,
            "t.co",
            &http,
            Url::parse("https://t.co/abc123").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "link");
        let request = server.join().unwrap().remove(0);
        assert!(request.starts_with("GET /abc123 "), "{request}");
        assert!(!request.contains("Mozilla"), "{request}");
    }

    #[tokio::test]
    async fn unresolved_links_fail() {
        let page = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (root, server) = serve(vec![page.to_vec()]);
        let http = http("Bitly", root);
        let events = extract(
            &BITLY,
            "bit.ly",
            &http,
            Url::parse("https://bit.ly/x").unwrap(),
        );
        let results: Vec<_> =
            futures_util::StreamExt::collect(crate::event::media_only(events)).await;
        assert!(results.iter().any(Result::is_err));
        let request = server.join().unwrap().remove(0);
        assert!(request.contains("Mozilla"), "{request}");
    }
}
