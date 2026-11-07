//! The `noop` input, mirroring gallery_dl/extractor/noop.py: it extracts
//! nothing (upstream uses it to store cookies without a download).
use crate::sites::{Events, Http, Route, Site};
use url::Url;

static NOOP: Route = Route::new("", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "noop",
    name: "noop",
    matches: |url| url.scheme() == "noop",
    extract,
    routes: &[&NOOP],
    api_key: false,
    cookies: None,
};

fn extract(_: &Http, _: Url) -> Events<'_> {
    crate::sites::events(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn extracts_nothing() {
        let url = crate::extract::http_url("NoOp").unwrap();
        assert!(crate::sites::find(&url).is_some_and(|site| site.category == "noop"));
        let http = crate::sites::testing::http("noop", url.clone());
        assert!(
            crate::sites::testing::media(extract(&http, url))
                .await
                .is_empty()
        );
    }
}
