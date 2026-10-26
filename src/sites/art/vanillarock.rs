//! vanilla-rock.com posts and tag/category listings, mirroring
//! gallery_dl/extractor/vanillarock.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, group, nameext_into, parse_iso, re, split_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://vanilla-rock.com";

static POST: Route = Route::new(
    "post",
    "{num:>02}.{extension}",
    &["{category}", "{path}"],
    "{filename}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "vanillarock",
    name: "Vanilla Rock",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &TAG],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    Tag(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?vanilla-rock\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}(/[^/?#]+)/?$")), url) {
        // Upstream's `(?!category/|tag/)`.
        let path = group(&c, 1);
        if !(url.path().starts_with("/category/") || url.path().starts_with("/tag/")) {
            return Some(Target::Post(path.into()));
        }
    }
    captures(re!(&format!(r"{BASE}(/(?:tag|category)/[^?#]+)")), url)
        .map(|c| Target::Tag(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Vanilla Rock URL"));
    };
    match target {
        Target::Post(path) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
            let mut scan = Scan::new(&page);
            let name = scan.extr("<h1 class=\"entry-title\">", "<").to_owned();
            let mut images = Vec::new();
            loop {
                let img = scan.extr("<div class=\"main-img\">", "</div>");
                if img.is_empty() {
                    break;
                }
                images.push(extr(img, "href=\"", "\"").to_owned());
            }
            let mut data = Map::new();
            data.insert("count".into(), images.len().into());
            data.insert("title".into(), unescape(&name).into());
            data.insert("path".into(), path.trim_matches('/').into());
            data.insert(
                "date".into(),
                date(parse_iso(scan.extr("<div class=\"date\">", "</div>"))),
            );
            let tags: Vec<Value> = split_html(scan.extr("<div class=\"cat-tag\">", "</div>"))
                .into_iter()
                .step_by(2)
                .map(Value::from)
                .collect();
            data.insert("tags".into(), tags.into());
            let items = images
                .into_iter()
                .enumerate()
                .filter_map(|(index, url)| {
                    let mut fields = data.clone();
                    fields.insert("num".into(), (index + 1).into());
                    nameext_into(&url, &mut fields);
                    Some(
                        Item::new(Url::parse(&url).ok()?, fields)
                            .ids(path.clone(), (index + 1).to_string()),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Tag(path) => {
            crate::sites::paginate(Some(format!("{ROOT}{path}")), move |state| async move {
                let Some(url) = state else { return Ok(None) };
                let page = http.text(&http.api(&url)?).await?;
                let mut scan = Scan::new(&page);
                let mut children = Vec::new();
                loop {
                    let post = scan.extr("<h2 class=\"entry-title\">", "</h2>");
                    if post.is_empty() {
                        break;
                    }
                    if let Ok(link) = Url::parse(extr(post, "href=\"", "\"")) {
                        children.push((link, Map::new()));
                    }
                }
                let next = unescape(scan.extr("class=\"next page-numbers\" href=\"", "\""));
                Ok(Some((
                    crate::sites::galleries::queue(http, &SITE, &TAG, children),
                    (!next.is_empty()).then_some(next),
                )))
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
            t("https://vanilla-rock.com/mizuhashi_parsee-5"),
            Some(Target::Post("/mizuhashi_parsee-5".into()))
        );
        assert_eq!(t("https://vanilla-rock.com/tag/"), None);
        assert_eq!(
            t("https://vanilla-rock.com/tag/%e5%b0%84%e5%91%bd%e4%b8%b8%e6%96%87"),
            Some(Target::Tag(
                "/tag/%e5%b0%84%e5%91%bd%e4%b8%b8%e6%96%87".into()
            ))
        );
        assert_eq!(
            t(
                "https://vanilla-rock.com/category/%e4%ba%8c%e6%ac%a1%e3%82%a8%e3%83%ad%e7%94%bb%e5%83%8f/a"
            ),
            Some(Target::Tag(
                "/category/%e4%ba%8c%e6%ac%a1%e3%82%a8%e3%83%ad%e7%94%bb%e5%83%8f/a".into()
            ))
        );
    }

    #[tokio::test]
    async fn extracts_post_images() {
        let page = r#"<h1 class="entry-title">Title</h1><div class="main-img"><a href="https://vanilla-rock.com/img/a.jpg"></a></div>
            <div class="main-img"><a href="https://vanilla-rock.com/img/b.jpg"></a></div><div class="date">2017-02-04</div><div class="cat-tag"><a>One</a>, <a>Two</a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("vanillarock", root);
        let files = media(extract(
            &http,
            Url::parse("https://vanilla-rock.com/post-1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            keywords(&files[1])["tags"],
            serde_json::json!(["One", "Two"])
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "02.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("vanillarock/post-1")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn tag_follows_next_page() {
        let first = r#"<h2 class="entry-title"><a href="https://vanilla-rock.com/a">A</a></h2><a class="next page-numbers" href="https://vanilla-rock.com/tag/x/page/2">"#;
        let second = r#"<h2 class="entry-title"><a href="https://vanilla-rock.com/b">B</a></h2>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("vanillarock", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://vanilla-rock.com/tag/x").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
