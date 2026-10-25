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

/// Configured `extractor.urlshortener.<category>` instances.
pub(crate) static FAMILY: crate::sites::instances::Family = crate::sites::instances::Family {
    basecategory: "urlshortener",
    builtin: &[&BITLY, &TCO],
    related: &[],
    template: &BITLY,
    matches: |category, url| {
        crate::sites::instances::configured(&FAMILY, category)
            .is_some_and(|instance| target(url, instance.spec).is_some())
    },
    extract: |category, http, url| match crate::sites::instances::configured(&FAMILY, category) {
        Some(instance) => extract(&instance.site, instance.spec, http, url),
        None => crate::sites::fail(error("unsupported short link")),
    },
};

/// `BASE_PATTERN + r"(/[^/?#]+)"` on the instance's `host[:port][/prefix]`.
fn target(url: &Url, spec: &str) -> Option<String> {
    let rest = crate::sites::instances::below(url, spec)?;
    let path = rest.split('?').next().unwrap_or_default();
    let segment = path.strip_prefix('/')?.split('/').next()?;
    (!segment.is_empty() && segment.len() <= 200).then(|| segment.to_owned())
}

fn extract<'a>(site: &'static Site, host: &str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(segment) = target(&url, host) else {
        return crate::sites::fail(error("unsupported short link"));
    };
    let configured = crate::sites::instances::configured(&FAMILY, site.category);
    let root = match configured {
        Some(instance) => format!("{}/{segment}", instance.root),
        None => format!("https://{host}/{segment}"),
    };
    // Upstream `headers` instance option; `{"User-Agent": null}` keeps the
    // plain default User-Agent.
    let custom: Option<Vec<(String, Option<String>)>> = configured
        .and_then(|instance| instance.config("headers"))
        .and_then(serde_json::Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .take(32)
                .filter(|(name, _)| {
                    !name.is_empty() && !name.contains([':', '\r', '\n']) && name.len() <= 128
                })
                .map(|(name, value)| {
                    let value = value
                        .as_str()
                        .filter(|v| !v.contains(['\r', '\n']))
                        .map(str::to_owned);
                    (name.clone(), value)
                })
                .collect()
        });
    crate::sites::once(async move {
        let request = http.api(&root)?;
        // t.co answers browsers with a 200 refresh page instead of a
        // redirect, so it keeps the plain default User-Agent.
        let plain_agent = match &custom {
            Some(headers) => headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("user-agent") && value.is_none()),
            None => site.category == "tco",
        };
        let extra: Vec<(&str, &str)> = custom
            .iter()
            .flatten()
            .filter_map(|(name, value)| Some((name.as_str(), value.as_deref()?)))
            .collect();
        let headers = if plain_agent {
            extra
        } else {
            http.browser_headers(&extra)
        };
        let location = http
            .location(&request, &headers)
            .await?
            .filter(|location| {
                configured.map_or_else(
                    || crate::sites::plain(location),
                    |instance| crate::sites::instances::plain_or_root(location, instance.root),
                )
            })
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
    async fn configured_instances_resolve_on_their_root() {
        let instance = crate::sites::instances::define(
            "urlshortener",
            "urlshortener-configured",
            serde_json::json!({
                "root": "https://short.test",
                "pattern": r"(?:www\.)?short\.test|sh\.test",
                "headers": {"User-Agent": null, "X-Test": "1"},
            }),
        );
        let site = crate::sites::find(&Url::parse("https://sh.test/abc").unwrap()).unwrap();
        assert_eq!(site.category, "urlshortener-configured");
        assert!(
            crate::sites::find(&Url::parse("https://sh.test/").unwrap())
                .is_none_or(|site| site.category != "urlshortener-configured")
        );
        let (root, server) = serve(vec![moved("https://speakerdeck.com/u/deck")]);
        let http = http("short", root);
        let children = queued((instance.site.extract)(
            &http,
            Url::parse("https://www.short.test/abc?x=1").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "link");
        let request = server.join().unwrap().remove(0);
        assert!(request.starts_with("GET /abc "), "{request}");
        assert!(request.contains("x-test: 1"), "{request}");
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
