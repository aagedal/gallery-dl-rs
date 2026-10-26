//! Idol Complex tag searches, pools and posts, mirroring
//! gallery_dl/extractor/idolcomplex.py: the Sankaku API at
//! `i.sankakuapi.com` with the idolcomplex.com origin.
use super::sankaku::{Flavor, extract_flavor, target};
use crate::sites::{Route, Site};

const FILENAME: &str = "{category}_{id}_{md5}.{extension}";

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POOL: Route = Route::new(
    "pool",
    FILENAME,
    &["{category}", "pool", "{pool[id]} {pool[name_en]}"],
    "p_{pool}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

pub static SITE: Site = Site {
    category: "idolcomplex",
    name: "Idol Complex",
    matches: |url| target(&IDOLCOMPLEX, url).is_some(),
    extract: |http, url| extract_flavor(&IDOLCOMPLEX, http, url),
    routes: &[&TAG, &POOL, &POST],
    api_key: false,
    cookies: Some("idolcomplex.com"),
};

static IDOLCOMPLEX: Flavor = Flavor {
    site: &SITE,
    api: "https://i.sankakuapi.com",
    root: "https://www.idolcomplex.com",
    base: r"^https?://(?:www\.)?idol(?:\.sankaku)?complex\.com(?:/[a-z]{2}(?:[-_][A-Z]{2})?)?",
    pool: r"/pools?/(?:show/)?(\w+)",
    books: false,
};

#[cfg(test)]
mod tests {
    use super::super::sankaku::Target;
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use url::Url;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&IDOLCOMPLEX, &Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.idolcomplex.com/en/posts?tags=lyumos"),
            Some(Target::Tag("tags=lyumos".into()))
        );
        assert_eq!(
            t("https://idol.sankakucomplex.com/?tags=lyumos+wreath&page=3"),
            Some(Target::Tag("tags=lyumos+wreath&page=3".into()))
        );
        assert_eq!(
            t("https://idol.sankakucomplex.com/en/pools/e9PMwnwRBK3"),
            Some(Target::Pool("e9PMwnwRBK3".into()))
        );
        assert_eq!(
            t("https://idol.sankakucomplex.com/pool/show/145"),
            Some(Target::Pool("145".into()))
        );
        assert_eq!(
            t("https://idol.sankakucomplex.com/post/show/694215"),
            Some(Target::Post("694215".into()))
        );
        assert_eq!(t("https://www.idolcomplex.com/books?tags=x"), None);
    }

    #[tokio::test]
    async fn uses_idol_api() {
        let (root, server) = serve(vec![json(
            serde_json::json!([{"id": "p1", "md5": "m", "file_url": "https://is.sankakucomplex.com/a.jpg",
            "created_at": {"s": 1600000000}, "tag_names": []}]),
        )]);
        let http = client("idolcomplex", root);
        let files = media((SITE.extract)(
            &http,
            Url::parse("https://www.idolcomplex.com/en/posts/p1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].category, "idolcomplex");
        assert_eq!(keywords(&files[0])["md5"], "m");
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("origin: https://www.idolcomplex.com")
        );
    }
}
