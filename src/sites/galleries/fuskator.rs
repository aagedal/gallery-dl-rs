//! Fuskator galleries and searches, mirroring gallery_dl/extractor/fuskator.py.
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, group, parse_int, re, rextr, rpartition, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://fuskator.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "fuskator",
    name: "Fuskator",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(r"^https?://fuskator\.com/(?:thumbs|expanded)/([^/?#]+)"),
        url,
    ) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    captures(re!(r"^https?://fuskator\.com(/(?:search|page)/.+)"), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fuskator URL"));
    };
    match target {
        Target::Gallery(hash) => crate::sites::once(async move {
            let page_url = format!("{ROOT}/thumbs/{hash}/index.html");
            let page = http.text(&http.api(&page_url)?).await?;
            let headers = [
                ("Referer", page_url.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ];
            let auth = match http
                .post_or_location(&http.api(&format!("{ROOT}/ajax/auth.aspx"))?, &headers)
                .await?
            {
                Ok(text) => text,
                Err(_) => return Err(error("Fuskator authorization failed")),
            };
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut gal = http.api(&format!("{ROOT}/ajax/gal.aspx"))?;
            gal.query_pairs_mut()
                .append_pair("X-Auth", &auth)
                .append_pair("hash", &hash)
                .append_pair("_", &ts.to_string());
            let json = http.json_with(&gal, &headers).await?;
            let (title, _, gallery_id) = rpartition(extr(&page, "<title>", "</title>").trim(), "#");
            let title: String = title
                .chars()
                .take(title.chars().count().saturating_sub(15))
                .collect();
            let mut data = Map::new();
            data.insert("gallery_id".into(), parse_int(gallery_id).into());
            data.insert("gallery_hash".into(), hash.as_str().into());
            data.insert("title".into(), unescape(&title).into());
            data.insert(
                "views".into(),
                json.get("hits").cloned().unwrap_or(Value::Null),
            );
            data.insert(
                "score".into(),
                json.get("rating").cloned().unwrap_or(Value::Null),
            );
            let tags: Vec<String> = json
                .get("tags")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split(',')
                .map(str::to_owned)
                .collect();
            data.insert("tags".into(), tags.into());
            let images = json["images"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|image| {
                    (
                        format!("https:{}", crate::sites::scalar(&image["imageUrl"])),
                        image.as_object().cloned(),
                    )
                })
                .collect();
            Ok(crate::sites::events(super::numbered(
                &SITE, http, &GALLERY, &data, images, &hash,
            )))
        }),
        Target::Search(path) => {
            crate::sites::paginate(Some(format!("{ROOT}{path}")), move |state| async move {
                let Some(url) = state else { return Ok(None) };
                let page = http.text(&http.api(&url)?).await?;
                let galleries = extract_iter(&page, "class=\"pic_pad\"><a href=\"", "\"")
                    .into_iter()
                    .filter_map(|path| {
                        Some((Url::parse(&format!("{ROOT}{path}")).ok()?, Map::new()))
                    })
                    .collect();
                let pages = extr(&page, "class=\"pages\"><span>", ">&gt;&gt;<");
                let next = (!pages.is_empty())
                    .then(|| format!("{ROOT}{}", rextr(pages, "href=\"", "\"", None)));
                Ok(Some((super::queue(http, &SITE, &SEARCH, galleries), next)))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fuskator.com/expanded/gXpKzjgIidA/index.html"),
            Some(Target::Gallery("gXpKzjgIidA".into()))
        );
        assert_eq!(
            t("https://fuskator.com/page/3/swimsuit/quality/"),
            Some(Target::Search("/page/3/swimsuit/quality/".into()))
        );
    }

    #[tokio::test]
    async fn extracts_gallery_through_ajax() {
        let (root, server) = serve(vec![
            html("<title> Red Swimsuit - Fuskator.com#123 </title>"),
            html("AUTH"),
            json(
                serde_json::json!({"hits": 5, "rating": 4.5, "tags": "red,swimsuit", "images": [{"imageUrl": "//i.fuskator.com/a/1.jpg", "width": 10}]}),
            ),
        ]);
        let http = client("fuskator", root);
        let files = media(extract(
            &http,
            Url::parse("https://fuskator.com/thumbs/d0GnIzXrSKU/").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://i.fuskator.com/a/1.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["gallery_id"], 123);
        assert_eq!(k["title"], "Red Swimsuit");
        assert_eq!(k["tags"], serde_json::json!(["red", "swimsuit"]));
        assert_eq!(k["width"], 10);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /ajax/auth.aspx "));
        assert!(requests[2].starts_with("GET /ajax/gal.aspx?X-Auth=AUTH&hash=d0GnIzXrSKU&_="));
    }

    #[tokio::test]
    async fn search_follows_pages() {
        let (root, server) = serve(vec![
            html(
                r#"<div class="pic_pad"><a href="/thumbs/a/">x</a></div><div class="pages"><span>1</span><a href="/page/2/red/">&gt;&gt;</a>"#,
            ),
            html(r#"<div class="pic_pad"><a href="/thumbs/b/">x</a></div>"#),
        ]);
        let http = client("fuskator", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://fuskator.com/search/red/").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /page/2/red/ "));
    }
}
