//! Fapello posts, models and listing paths, mirroring
//! gallery_dl/extractor/fapello.py (fapello.com and fapello.su).
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, group, nameext_into, parse_int, re, root_of,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const BASE: &str = r"^https?://(?:www\.)?fapello\.(?:com|su)";

static POST: Route = Route::new(
    "post",
    "{model}_{id}.{extension}",
    &["{category}", "{model}"],
    "{type}_{model}_{id}",
);
static MODEL: Route = Route::new("model", "{filename}.{extension}", &["{category}"], "");
static PATH: Route = Route::new("path", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "fapello",
    name: "Fapello",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &MODEL, &PATH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String),
    Model(String),
    Path(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/([^/?#]+)/(\d+)")), url) {
        return (!matches!(&c[1], "search" | "popular_videos"))
            .then(|| Target::Post(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(top-(?:likes|followers)|videos|trending|popular_videos/[^/?#]+)/?$"
        )),
        url,
    ) {
        return Some(Target::Path(group(&c, 1).into()));
    }
    let c = captures(re!(&format!(r"{BASE}/([^/?#]+)/?$")), url)?;
    let name = group(&c, 1);
    (!(name.starts_with("top-likes")
        || name.starts_with("top-followers")
        || name.starts_with("popular_videos")
        || name.starts_with("videos")
        || name.starts_with("trending")
        || name == "search"))
        .then(|| Target::Model(name.into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fapello URL"));
    };
    let root = root_of(&url);
    match target {
        Target::Post(model, id) => crate::sites::once(async move {
            let request = http.api(&format!("{root}/{model}/{id}/"))?;
            if http.location(&request, &[]).await?.is_some() {
                return Err(error("Fapello post not found"));
            }
            let page = http.text(&request).await?;
            let Some(block) = crate::sites::extr(&page, "class=\"uk-align-center\"", "</div>")
            else {
                return Err(error("Fapello post not found"));
            };
            let mut data = Map::new();
            data.insert("model".into(), model.as_str().into());
            data.insert("id".into(), parse_int(&id).into());
            data.insert(
                "type".into(),
                if block.contains("type=\"video") {
                    "video"
                } else {
                    "photo"
                }
                .into(),
            );
            data.insert("thumbnail".into(), extr(block, "poster=\"", "\"").into());
            let raw = extr(block, "src=\"", "\"")
                .replace(".md", "")
                .replace(".th", "");
            nameext_into(&raw, &mut data);
            let file = Url::parse(&raw).map_err(|_| error("Fapello post has no file"))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                vec![Item::new(file, data).ids(id.clone(), id)],
            )))
        }),
        Target::Model(model) => crate::sites::paginate(Some(1u64), move |state| {
            let root = root.clone();
            let model = model.clone();
            async move {
                let Some(number) = state else { return Ok(None) };
                let page = http
                    .text(&http.api(&format!("{root}/ajax/model/{model}/page-{number}/"))?)
                    .await?;
                let links: Vec<&str> = extract_iter(&page, "<a href=\"", "\"");
                if page.is_empty() || links.is_empty() {
                    return Ok(None);
                }
                let posts = links
                    .into_iter()
                    .filter(|u| *u != "javascript:void(0);")
                    .filter_map(|u| Some((Url::parse(u).ok()?, Map::new())))
                    .collect();
                Ok(Some((
                    super::queue(http, &SITE, &MODEL, posts),
                    Some(number + 1),
                )))
            }
        }),
        Target::Path(path) => {
            let path = if root.contains("fapello.su") {
                path.replace('-', "/")
            } else {
                path
            };
            crate::sites::paginate(Some(1u64), move |state| {
                let root = root.clone();
                let path = path.clone();
                async move {
                    let Some(number) = state else { return Ok(None) };
                    let page = http
                        .text(&http.api(&format!("{root}/ajax/{path}/page-{number}/"))?)
                        .await?;
                    if page.is_empty() {
                        return Ok(None);
                    }
                    let items = extract_iter(&page, "uk-transition-toggle\">", "</a>")
                        .into_iter()
                        .filter_map(|item| {
                            Some((Url::parse(extr(item, "<a href=\"", "\"")).ok()?, Map::new()))
                        })
                        .collect();
                    Ok(Some((
                        super::queue(http, &SITE, &PATH, items),
                        Some(number + 1),
                    )))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fapello.su/grace-charis/2038266/"),
            Some(Target::Post("grace-charis".into(), "2038266".into()))
        );
        assert_eq!(
            t("https://fapello.com/hyoon/"),
            Some(Target::Model("hyoon".into()))
        );
        assert_eq!(
            t("https://fapello.com/top-likes/"),
            Some(Target::Path("top-likes".into()))
        );
        assert_eq!(
            t("https://fapello.com/popular_videos/week/"),
            Some(Target::Path("popular_videos/week".into()))
        );
        assert_eq!(t("https://fapello.com/search/"), None);
    }

    #[tokio::test]
    async fn extracts_full_size_post() {
        let (root, server) = serve(vec![
            html("ok"),
            html(
                r#"<div class="uk-align-center"><img src="https://fapello.com/content/c/a/carrykey/1000/carrykey_0530.md.jpg"></div>"#,
            ),
        ]);
        let http = client("fapello", root);
        let files = media(extract(
            &http,
            Url::parse("https://fapello.com/carrykey/530/").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://fapello.com/content/c/a/carrykey/1000/carrykey_0530.jpg"
        );
        assert_eq!(keywords(&files[0])["type"], "photo");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "carrykey_530.jpg");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn model_pages_until_empty() {
        let (root, server) = serve(vec![
            html(r#"<a href="https://fapello.com/hyoon/1/"></a><a href="javascript:void(0);">"#),
            html(""),
        ]);
        let http = client("fapello", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://fapello.com/hyoon/").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /ajax/model/hyoon/page-1/ "));
    }
}
