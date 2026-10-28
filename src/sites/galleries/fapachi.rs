//! Fapachi posts and users, mirroring gallery_dl/extractor/fapachi.py.
use crate::error;
use crate::sites::gdl::{Scan, captures, extr, extract_iter, group, nameext_into, re};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://fapachi.com";

static POST: Route = Route::new(
    "post",
    "{user}_{id}.{extension}",
    &["{category}", "{user}"],
    "{user}_{id}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "fapachi",
    name: "Fapachi",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String),
    User(String, i64),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(r"^https?://(?:www\.)?fapachi\.com/([^/?#]+)/media/(\d+)"),
        url,
    ) {
        return (&c[1] != "search").then(|| Target::Post(group(&c, 1).into(), group(&c, 2).into()));
    }
    let c = captures(
        re!(r"^https?://(?:www\.)?fapachi\.com/([^/?#]+)(?:/page/(\d+))?$"),
        url,
    )?;
    (&c[1] != "search").then(|| {
        Target::User(
            group(&c, 1).into(),
            crate::sites::art::parse_int(group(&c, 2), 1),
        )
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fapachi URL"));
    };
    match target {
        Target::Post(user, id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/{user}/media/{id}"))?)
                .await?;
            let start = page
                .find("class=\"media-img")
                .ok_or_else(|| error("Fapachi post has no media"))?;
            let raw = format!("{ROOT}{}", Scan::at(&page, start).extr("data-src=\"", "\""));
            let mut data = Map::new();
            data.insert("user".into(), user.as_str().into());
            data.insert("id".into(), id.as_str().into());
            nameext_into(&raw, &mut data);
            let file = Url::parse(&raw).map_err(|_| error("invalid Fapachi media URL"))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                vec![Item::new(file, data).ids(id.clone(), id)],
            )))
        }),
        Target::User(user, first) => crate::sites::paginate(Some(first), move |state| {
            let user = user.clone();
            async move {
                let Some(number) = state else { return Ok(None) };
                let page = http
                    .text(&http.api(&format!("{ROOT}/{user}/page/{number}"))?)
                    .await?;
                let posts = extract_iter(&page, "model-media-prew\">", ">")
                    .into_iter()
                    .map(|post| extr(post, "<a href=\"", "\""))
                    .filter(|path| !path.is_empty())
                    .filter_map(|path| {
                        Some((Url::parse(&format!("{ROOT}{path}")).ok()?, Map::new()))
                    })
                    .collect();
                let next = page.contains("\">Next page</a>").then_some(number + 1);
                Ok(Some((super::queue(http, &SITE, &USER, posts), next)))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fapachi.com/sonson/media/0082"),
            Some(Target::Post("sonson".into(), "0082".into()))
        );
        assert_eq!(
            t("https://fapachi.com/sonson/page/3"),
            Some(Target::User("sonson".into(), 3))
        );
        assert_eq!(t("https://fapachi.com/search"), None);
        assert_eq!(t("https://fapachi.com/search/media/1"), None);
    }

    #[tokio::test]
    async fn extracts_post_and_pages_user() {
        let (root, server) = serve(vec![html(
            r#"<div class="media-img"><img data-src="/models/s/o/sonson/1/full/sonson_0082.jpeg">"#,
        )]);
        let http = client("fapachi", root);
        let files = media(extract(
            &http,
            Url::parse("https://fapachi.com/sonson/media/0082").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://fapachi.com/models/s/o/sonson/1/full/sonson_0082.jpeg"
        );
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "sonson_0082.jpg");
        server.join().unwrap();

        let (root, server) = serve(vec![
            html(r#"<div class="model-media-prew"><a href="/sonson/media/1"></a>">Next page</a>"#),
            html(r#"<div class="model-media-prew"><a href="/sonson/media/2"></a>"#),
        ]);
        let http = client("fapachi", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://fapachi.com/sonson").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
