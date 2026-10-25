//! URL lists behind `recursive:URL` (or `r:URL`) inputs, mirroring
//! gallery_dl/extractor/recursive.py: every `https?://` run in the fetched
//! page is queued for whichever extractor handles it. Local `file://`
//! sources are rejected when the input is parsed; `--input-file` reads them.
use crate::error;
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

static RECURSIVE: Route = Route::new("", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "recursive",
    name: "recursive",
    matches: |url| url.scheme() == "recursive" && crate::extract::prefixed_target(url).is_some(),
    extract,
    routes: &[&RECURSIVE],
    api_key: false,
    cookies: None,
};

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(source) = crate::extract::prefixed_target(&url) else {
        return crate::sites::fail(error("unsupported recursive URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(source.as_str())?).await?;
        let children: Vec<Url> = links(&page)
            .into_iter()
            .filter_map(|link| Url::parse(link).ok())
            .collect();
        Ok(futures_util::StreamExt::boxed(
            futures_util::StreamExt::flatten(futures_util::stream::iter(children.into_iter().map(
                move |child| {
                    crate::sites::queue(
                        http,
                        SITE.category,
                        RECURSIVE.subcategory,
                        child,
                        &Map::new(),
                    )
                },
            ))),
        ))
    })
}

/// `re(r"https?://[^\s\"']+").finditer(page)`, case-sensitive like upstream.
fn links(page: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = page;
    while let Some(at) = rest.find("http") {
        let after = &rest[at + 4..];
        let scheme = if after.starts_with("://") {
            Some(3)
        } else if after.starts_with("s://") {
            Some(4)
        } else {
            None
        };
        let Some(scheme) = scheme else {
            rest = after;
            continue;
        };
        let body = &after[scheme..];
        let len = body
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(body.len());
        if len == 0 {
            rest = body;
            continue;
        }
        found.push(&rest[at..at + 4 + scheme + len]);
        rest = &body[len..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, queued, serve};

    #[test]
    fn parses_inputs() {
        let url = crate::extract::input_url("r:https://pastebin.com/raw/FLwrCYsT").unwrap();
        assert_eq!(url.as_str(), "recursive:https://pastebin.com/raw/FLwrCYsT");
        assert!((SITE.matches)(&url));
        assert!((SITE.matches)(
            &crate::extract::input_url("recursive:example.org/list").unwrap()
        ));
        // Upstream's prefix is case-sensitive.
        assert!(crate::extract::input_url("R:https://example.org/").is_err());
        assert!(crate::extract::input_url("r:file:///etc/hosts").is_err());
    }

    #[test]
    fn finds_links() {
        let page = "a https://x.test/1 \"http://y.test/2\" 'https://z.test/3?a=b#c'\n\
            HTTPS://upper.test/no httpx://no https:// https://end.test/4";
        assert_eq!(
            links(page),
            vec![
                "https://x.test/1",
                "http://y.test/2",
                "https://z.test/3?a=b#c",
                "https://end.test/4",
            ]
        );
    }

    #[tokio::test]
    async fn queues_every_link() {
        let (root, server) = serve(vec![html(
            "https://speakerdeck.com/u/a\nhttps://cdn.example.test/b.png\n",
        )]);
        let http = http("recursive", root);
        let url = crate::extract::input_url("r:https://pastebin.com/raw/list").unwrap();
        let children = queued(extract(&http, url)).await;
        assert_eq!(children.len(), 2);
        let request = server.join().unwrap().remove(0);
        assert!(request.starts_with("GET /raw/list "), "{request}");
    }
}
