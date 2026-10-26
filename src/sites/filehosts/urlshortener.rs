//! General-purpose URL shorteners (bit.ly, t.co), mirroring
//! gallery_dl/extractor/urlshortener.py: the redirect target is queued for
//! whichever extractor reads it.
use crate::error;
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

static LINK: Route = Route::new("link", "{filename}.{extension}", &["{category}"], "");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $host:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($host, url).is_some(),
            extract: |http, url| extract($category, $host, http, url),
            routes: &[&LINK],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(BITLY, "bitly", "Bitly", "bit.ly");
instance!(TCO, "tco", "t.co", "t.co");

/// `(/[^/?#]+)` below the shortener's host.
fn target(host: &str, url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some(host) {
        return None;
    }
    let path = url.path();
    let segment = path.strip_prefix('/')?.split('/').next()?;
    (!segment.is_empty()).then(|| format!("/{segment}"))
}

fn extract<'a>(category: &'static str, host: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(path) = target(host, &url) else {
        return crate::sites::fail(error("unsupported short URL"));
    };
    crate::sites::once(async move {
        let location = http
            .location(&http.api(&format!("https://{host}{path}"))?, &[])
            .await?
            .ok_or_else(|| error("Unable to resolve short URL"))?;
        Ok(crate::sites::queue(
            http,
            category,
            LINK.subcategory,
            location,
            &Map::new(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, response, serve};
    use futures_util::StreamExt;

    #[test]
    fn classifies_urls() {
        let t = |host: &str, raw: &str| target(host, &Url::parse(raw).unwrap());
        assert_eq!(
            t("bit.ly", "https://bit.ly/3cWIUgq").as_deref(),
            Some("/3cWIUgq")
        );
        assert_eq!(
            t("t.co", "https://t.co/bCgBY8Iv5n?x=1").as_deref(),
            Some("/bCgBY8Iv5n")
        );
        assert_eq!(t("bit.ly", "https://bit.ly/"), None);
        assert_eq!(t("t.co", "https://www.t.co/x"), None);
    }

    #[tokio::test]
    async fn queues_redirect_targets_for_any_extractor() {
        let redirect = b"HTTP/1.1 301 Moved\r\nLocation: https://example.org/photo.jpg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (root, server) = serve(vec![redirect.to_vec()]);
        let http = client("bitly", root);
        let events: Vec<_> = (BITLY.extract)(&http, Url::parse("https://bit.ly/abc").unwrap())
            .collect()
            .await;
        // A target outside the registry is handed to the caller's dispatcher.
        let external = events.iter().find_map(|event| match event {
            Err(error) => error
                .downcast_ref::<crate::sites::ExternalChild>()
                .map(|c| c.0.to_string()),
            _ => None,
        });
        assert_eq!(external.as_deref(), Some("https://example.org/photo.jpg"));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /abc "));

        let (root, server) = serve(vec![response("200 OK", "text/html", b"")]);
        let http = client("bitly", root);
        assert!(
            crate::sites::testing::fails((BITLY.extract)(
                &http,
                Url::parse("https://bit.ly/abc").unwrap()
            ))
            .await
        );
        server.join().unwrap();
    }
}
