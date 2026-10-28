//! `recursive:`/`r:` inputs: queue every URL found in a remote page or a
//! local `file://` document, mirroring gallery_dl/extractor/recursive.py.
use crate::error;
use crate::sites::gdl::{ensure_http_scheme, re};
use crate::sites::{Events, Http, Route, Site};
use futures_util::{StreamExt, stream};
use serde_json::Map;
use url::Url;

static RECURSIVE: Route = Route::new("", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "recursive",
    name: "recursive",
    matches: |url| matches!(url.scheme(), "r" | "recursive"),
    extract,
    routes: &[&RECURSIVE],
    api_key: false,
    cookies: None,
};

/// Every `https?://` URL in `page`, in order.
fn urls(page: &str) -> Vec<String> {
    re!(r#"https?://[^\s"']+"#)
        .find_iter(page)
        .map(|m| m.as_str().to_owned())
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let source = url
        .as_str()
        .split_once(':')
        .map(|(_, rest)| rest.to_owned())
        .unwrap_or_default();
    crate::sites::once(async move {
        let page = match source.strip_prefix("file://") {
            Some(path) => tokio::fs::read_to_string(path)
                .await
                .map_err(|_| error("cannot read recursive input file"))?,
            None => {
                http.text(&Url::parse(&ensure_http_scheme(&source))?)
                    .await?
            }
        };
        let children: Vec<Events<'_>> = urls(&page)
            .into_iter()
            .filter_map(|child| Url::parse(&child).ok())
            .map(|child| crate::sites::queue(http, SITE.category, "", child, &Map::new()))
            .collect();
        Ok(stream::iter(children).flatten().boxed())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls_like_upstream() {
        assert_eq!(
            urls("a https://x.test/1 \"http://y.test/2?q=1\" 'https://z.test/3'\nftp://no"),
            [
                "https://x.test/1",
                "http://y.test/2?q=1",
                "https://z.test/3"
            ]
        );
        assert!((SITE.matches)(
            &Url::parse("recursive:https://x.test/").unwrap()
        ));
        assert!((SITE.matches)(&Url::parse("r:file:///tmp/list").unwrap()));
        assert!(!(SITE.matches)(&Url::parse("https://x.test/").unwrap()));
    }
}
