//! thefap.net posts and models, mirroring gallery_dl/extractor/thefap.py.
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, group, nameext_into, parse_int, re, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://thefap.net";
const BASE: &str = r"^https?://(?:www\.)?thefap\.net";
const FILENAME: &str = "{model}_{num:>03}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{model_name} ({model_id})"];

static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{model_id}_{filename}");
static MODEL: Route = Route::new("model", FILENAME, DIRECTORY, "{model_id}_{filename}");

pub static SITE: Site = Site {
    category: "thefap",
    name: "TheFap",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &MODEL],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Path, model, model id, kind, post id.
    Post(String, String, String, String, String),
    Model(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(r"{BASE}(/([^/?#]+)-(\d+)/([^/?#]+)/i(\d+))")),
        url,
    ) {
        return Some(Target::Post(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
            group(&c, 4).into(),
            group(&c, 5).into(),
        ));
    }
    captures(re!(&format!(r"{BASE}/([^/?#]+)-(\d+)")), url)
        .map(|c| Target::Model(group(&c, 1).into(), group(&c, 2).into()))
}

/// `_normalize_url`.
fn normalize(url: &str) -> String {
    let mut url = url.trim().to_owned();
    if url.contains("?w=") {
        url.truncate(url.rfind('?').unwrap_or(url.len()));
    } else if let Some(stem) = url.strip_suffix(":small") {
        url = format!("{stem}:orig");
    }
    if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("{ROOT}{url}")
    } else {
        url
    }
}

fn numbered(
    data: &Map<String, Value>,
    urls: Vec<&str>,
    start: usize,
    id: &str,
) -> (Vec<Item>, usize) {
    let mut num = start;
    let items = urls
        .into_iter()
        .map(normalize)
        .filter(|url| !url.is_empty())
        .filter_map(|url| {
            let parsed = Url::parse(&url).ok()?;
            num += 1;
            let mut fields = data.clone();
            fields.insert("num".into(), num.into());
            nameext_into(&url, &mut fields);
            Some(Item::new(parsed, fields).ids(id, num.to_string()))
        })
        .collect();
    (items, num)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported TheFap URL"));
    };
    match target {
        Target::Post(path, model, model_id, kind, post_id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
            if page.contains("Not Found") {
                return Err(error("TheFap post not found"));
            }
            let name = extr(&page, "<title>", " / ");
            let name = if name.is_empty() {
                unquote(&model).replace('.', " ")
            } else {
                unescape(name)
            };
            let mut data = Map::new();
            data.insert("model".into(), model.as_str().into());
            data.insert("model_id".into(), parse_int(&model_id).into());
            data.insert("model_name".into(), name.into());
            data.insert("kind".into(), kind.into());
            data.insert("post_id".into(), parse_int(&post_id).into());
            let header = page
                .find("</header>")
                .ok_or_else(|| error("TheFap post has no header"))?;
            let body = extr(&page[header..], "\n</div>", "\n<!---->");
            let (items, _) = numbered(&data, extract_iter(body, "<img src=\"", "\""), 0, &post_id);
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Model(model, model_id) => crate::sites::paginate(
            Some((1u64, 0usize, None::<Map<String, Value>>)),
            move |state| {
                let model = model.clone();
                let model_id = model_id.clone();
                async move {
                    let Some((page_num, num, data)) = state else {
                        return Ok(None);
                    };
                    let (data, urls_page, needle) = match data {
                        Some(data) => {
                            let headers = [
                                ("X-Requested-With", "XMLHttpRequest"),
                                ("Sec-Fetch-Dest", "empty"),
                                ("Sec-Fetch-Mode", "cors"),
                                ("Sec-Fetch-Site", "same-origin"),
                            ];
                            let page = http
                                .text_with(
                                    &http.api(&format!(
                                        "{ROOT}/ajax/model/{model_id}/page-{page_num}"
                                    ))?,
                                    &headers,
                                )
                                .await?;
                            if page.is_empty() {
                                return Ok(None);
                            }
                            (data, page, "<img src=\"")
                        }
                        None => {
                            let page = http
                                .text(&http.api(&format!("{ROOT}/{model}-{model_id}/"))?)
                                .await?;
                            if !page.contains("id=\"content\"") {
                                return Err(error("TheFap model not found"));
                            }
                            let heading = extr(&page, "<h2", "</h2>");
                            let name = if heading.is_empty() {
                                unquote(&model).replace('.', " ")
                            } else {
                                unescape(&heading[heading.find('>').map_or(0, |i| i + 1)..])
                            };
                            let mut data = Map::new();
                            data.insert("model".into(), model.as_str().into());
                            data.insert("model_id".into(), parse_int(&model_id).into());
                            data.insert("model_name".into(), name.into());
                            (
                                data,
                                extr(&page, "<div id=\"content\"", "<div id=\"showmore\"")
                                    .to_owned(),
                                "data-src=\"",
                            )
                        }
                    };
                    let (items, num) = numbered(
                        &data,
                        extract_iter(&urls_page, needle, "\""),
                        num,
                        &model_id,
                    );
                    let events = items
                        .into_iter()
                        .flat_map(|item| crate::sites::post(&SITE, http, &MODEL, vec![item]))
                        .collect();
                    Ok(Some((
                        crate::sites::events(events),
                        Some((page_num + 1, num, Some(data))),
                    )))
                }
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, media, serve};

    #[test]
    fn normalizes_urls() {
        assert_eq!(normalize(" https://x/a.jpg?w=300 "), "https://x/a.jpg");
        assert_eq!(
            normalize("https://pbs.twimg.com/media/a.jpg:small"),
            "https://pbs.twimg.com/media/a.jpg:orig"
        );
        assert_eq!(normalize("//cdn/a.jpg"), "https://cdn/a.jpg");
        assert_eq!(normalize("/img/a.jpg"), "https://thefap.net/img/a.jpg");
    }

    #[tokio::test]
    async fn model_pages_until_empty() {
        let page = r#"<h2 class="x">Jane Doe</h2><div id="content"><img data-src="/a.jpg"><div id="showmore">"#;
        let (root, server) = serve(vec![
            html(page),
            html(r#"<img src="https://cdn/b.jpg?w=1">"#),
            html(""),
        ]);
        let http = client("thefap", root);
        let files = media(extract(
            &http,
            Url::parse("https://thefap.net/jane.doe-123/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://cdn/b.jpg");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "jane.doe_002.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("thefap/Jane Doe (123)")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /ajax/model/123/page-2 "));
    }
}
