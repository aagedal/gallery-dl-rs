//! Girls with Muscle posts and searches, mirroring
//! gallery_dl/extractor/girlswithmuscle.py.
use crate::error;
use crate::sites::gdl::{
    auth_info, captures, date, extr, extract_iter, group, nameext_into, parse_int, parse_iso, re,
    remove_html, split_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.girlswithmuscle.com";
const BASE: &str = r"^https?://(?:www\.)?girlswithmuscle\.com";

static POST: Route = Route::new(
    "post",
    "{model}_{id}.{extension}",
    &["{category}", "{model}"],
    "{type}_{model}_{id}",
);
static SEARCH: Route = Route::new(
    "search",
    "{model}_{id}.{extension}",
    &["{category}", "{model}"],
    "{type}_{model}_{id}",
);

pub static SITE: Site = Site {
    category: "girlswithmuscle",
    name: "Girls with Muscle",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &SEARCH],
    api_key: false,
    cookies: Some("girlswithmuscle.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/(\d+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    let start = captures(re!(&format!(r"{BASE}/images/")), url)?
        .get(0)?
        .end();
    let rest = &url.as_str()[start..];
    Some(Target::Search(
        rest.split('#').next().unwrap_or_default().into(),
    ))
}

/// `login`: POST the login form; success redirects.
async fn login(http: &Http) -> crate::Result<()> {
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let url = http.api(&format!("{ROOT}/login/"))?;
    let page = http.text(&url).await?;
    let csrf = extr(&page, "name=\"csrfmiddlewaretoken\" value=\"", "\"");
    let form = [
        ("csrfmiddlewaretoken", csrf),
        ("username", username.as_str()),
        ("password", password.as_str()),
        ("next", "/"),
    ];
    match http
        .post_form_text(&url, &form, &[("Origin", ROOT), ("Referer", url.as_str())])
        .await
    {
        Err(e) if e.to_string().contains("redirected a POST") => Ok(()),
        Err(e) => Err(e),
        Ok(_) => Err(error(
            "Girls with Muscle login failed; check the username and password",
        )),
    }
}

/// `GirlswithmusclePostExtractor.metadata`.
fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let source = remove_html(extr(
        page,
        "<div id=\"info-source\" style=\"display: none\">",
        "</div>",
    ));
    let info = extr(page, "<div class=\"image-info\">", "</div>");
    let uploader = remove_html(extr(info, "<span class=\"username-html\">", "</a>"));
    let tags: Vec<Value> = split_html(extr(page, "id=\"tags-text\">", "</div>"))
        .into_iter()
        .skip(1)
        .step_by(2)
        .map(Value::from)
        .collect();
    let score = parse_int(&remove_html(extr(page, "Score: <b>", "</span")));
    let title = extr(page, "<title>", "</title>");
    let model = if title.starts_with("Picture #") {
        "unknown"
    } else {
        title
    };
    let models: Vec<Value> = if model == "unknown" {
        Vec::new()
    } else {
        model.split(',').map(|n| n.trim().into()).collect()
    };
    let stamp = extr(page, "class=\"hover-time\"  title=\"", "\"");
    let favorite = if extr(page, "id=\"favorite-button\">", "</span>") == "Favorite" {
        Value::Bool(false)
    } else if extr(page, "class=\"actionbutton unfavorite-button\">", "</span>") == "Unfavorite" {
        Value::Bool(true)
    } else {
        Value::Null
    };
    let comments: Vec<Value> = extract_iter(page, "<div class=\"comment-body-inner\">", "</div>")
        .into_iter()
        .map(|c| c.trim().into())
        .collect();
    let mut data = Map::new();
    data.insert("id".into(), id.into());
    data.insert("model".into(), model.into());
    data.insert("model_list".into(), models.into());
    data.insert("tags".into(), tags.into());
    data.insert(
        "date".into(),
        date(parse_iso(stamp.get(..19).unwrap_or(stamp))),
    );
    data.insert("is_favorite".into(), favorite);
    data.insert("source_filename".into(), source.into());
    data.insert("uploader".into(), uploader.into());
    data.insert("score".into(), score.into());
    data.insert("comments".into(), comments.into());
    data
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Girls with Muscle URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            login(http).await?;
            let page = http.text(&http.api(&format!("{ROOT}/{id}/"))?).await?;
            if page.is_empty() {
                return Err(error("Girls with Muscle post not found"));
            }
            let mut data = metadata(&page, &id);
            let mut file = extr(&page, "class=\"main-image\" src=\"", "\"");
            if file.is_empty() {
                file = extr(&page, "<source src=\"", "\"");
                data.insert("type".into(), "video".into());
            } else {
                data.insert("type".into(), "picture".into());
            }
            nameext_into(file, &mut data);
            let file = Url::parse(&unescape(file))
                .map_err(|_| error("Girls with Muscle post has no file"))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                vec![Item::new(file, data).ids(id.clone(), id)],
            )))
        }),
        Target::Search(query) => crate::sites::paginate(Some((1i64, 0i64)), move |state| {
            let query = query.clone();
            async move {
                let Some((number, total)) = state else {
                    return Ok(None);
                };
                let (page, total) = if number == 1 {
                    login(http).await?;
                    let request = http.api(&format!("{ROOT}/images/{query}"))?;
                    let (page, final_url) = http.text_final(&request, &[]).await?;
                    if final_url != request {
                        return Err(error(format!(
                            "Request was redirected to \"{final_url}\", try logging in"
                        )));
                    }
                    let Some(c) = re!(r"Page (\d+) of (\d+)").captures(&page) else {
                        return Err(error("Girls with Muscle search has no page count"));
                    };
                    let (current, total) = (parse_int(&c[1]), parse_int(&c[2]));
                    // Continue after the page the URL already points at.
                    return Ok(Some((
                        page_events(http, &page),
                        (current < total).then_some((current + 1, total)),
                    )));
                } else {
                    (
                        http.text(&http.api(&format!("{ROOT}/images/{number}/{query}"))?)
                            .await?,
                        total,
                    )
                };
                Ok(Some((
                    page_events(http, &page),
                    (number < total).then_some((number + 1, total)),
                )))
            }
        }),
    }
}

fn page_events<'a>(http: &'a Http, page: &str) -> Events<'a> {
    let mut data = Map::new();
    data.insert(
        "gallery_name".into(),
        unescape(extr(page, "<title>", "<")).into(),
    );
    let children = extract_iter(page, "id=\"imgid-", "\"")
        .into_iter()
        .filter_map(|id| Some((Url::parse(&format!("{ROOT}/{id}/")).ok()?, data.clone())))
        .collect();
    super::queue(http, &SITE, &SEARCH, children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.girlswithmuscle.com/12345/"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(
            t("https://www.girlswithmuscle.com/images/?name=Jane"),
            Some(Target::Search("?name=Jane".into()))
        );
    }

    #[tokio::test]
    async fn extracts_post_metadata() {
        let page = r#"<title>Jane Doe, Ann Roe</title><div id="tags-text">Tags <a>abs</a> | <a>legs</a></div> Score: <b>12</b></span>
            <img class="main-image" src="https://www.girlswithmuscle.com/images/full/12345.jpg">
            <span class="hover-time"  title="2024-01-02 03:04:05.123">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("girlswithmuscle", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.girlswithmuscle.com/12345/").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["type"], "picture");
        assert_eq!(k["model_list"], serde_json::json!(["Jane Doe", "Ann Roe"]));
        assert_eq!(k["tags"], serde_json::json!(["abs", "legs"]));
        assert_eq!(k["score"], 12);
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "Jane Doe, Ann Roe_12345.jpg"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn search_walks_pages() {
        let (root, server) = serve(vec![
            html(r#"<title>Jane</title>Page 1 of 2 <div id="imgid-1"></div>"#),
            html(r#"<div id="imgid-2"></div>"#),
        ]);
        let http = client("girlswithmuscle", root);
        let queued = children(extract(
            &http,
            Url::parse("https://www.girlswithmuscle.com/images/?name=Jane").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /images/2/?name=Jane "));
    }
}
