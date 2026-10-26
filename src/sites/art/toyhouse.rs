//! Toyhouse art galleries and images, mirroring gallery_dl/extractor/toyhouse.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extract_iter, group, nameext_into, partition, re, remove_html,
    split_html, strptime,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://toyhou.se";

static ART: Route = Route::new(
    "art",
    "{filename}.{extension}",
    &["{category}", "{user|artists!S}"],
    "{id}",
);
static IMAGE: Route = Route::new(
    "image",
    "{filename}.{extension}",
    &["{category}", "{user|artists!S}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "toyhouse",
    name: "Toyhouse",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ART, &IMAGE],
    api_key: false,
    cookies: Some("toyhou.se"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Art(String),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://(?:www\.)?toyhou\.se/([^/?#]+)/art"), url) {
        return Some(Target::Art(group(&c, 1).into()));
    }
    captures(re!(r"^https?://(?:(?:www\.)?toyhou\.se/~images|f\d+\.toyhou\.se/file/[^/?#]+/(?:image|watermark)s)/(\d+)"), url).map(|c| Target::Image(group(&c, 1).into()))
}

/// `_parse_post` plus the `items` filename split.
fn parse_post(post: &str, needle: &str, extra: &Map<String, Value>) -> Option<Item> {
    let mut scan = Scan::new(post);
    let url = scan.extr(needle, "\"").to_owned();
    let mut data = Map::new();
    data.insert("url".into(), url.as_str().into());
    data.insert(
        "date".into(),
        date(strptime(
            scan.extr("</h2>\n            <div class=\"mb-1\">", "<"),
            "%d %b %Y, %I:%M:%S %p",
        )),
    );
    let artists: Vec<Value> = scan
        .extr(
            "<div class=\"artist-credit\">",
            "</div>\n                    </div>",
        )
        .split("<div class=\"ar tist-credit\">")
        .map(|artist| remove_html(artist).into())
        .collect();
    data.insert("artists".into(), artists.into());
    let characters: Vec<Value> = split_html(scan.extr(
        "<div class=\"image-characters",
        "<div class=\"image-comments\">",
    ))
    .into_iter()
    .skip(2)
    .map(Value::from)
    .collect();
    data.insert("characters".into(), characters.into());
    data.extend(extra.clone());
    nameext_into(&url, &mut data);
    let filename = crate::sites::scalar(&data["filename"]);
    let (id, _, hash) = partition(&filename, "_");
    let id = id.to_owned();
    data.insert("id".into(), id.as_str().into());
    data.insert("hash".into(), hash.into());
    Some(Item::new(Url::parse(&url).ok()?, data).ids(id.clone(), id))
}

/// `_accept_content_warning`: POST the warning form; `false` without one.
async fn accept_warning(http: &Http, page: &str) -> bool {
    let Some(pos) = page.find(" name=\"_token\"") else {
        return false;
    };
    let mut scan = Scan::at(page, pos + 1);
    let token = scan.extr(" value=\"", "\"").to_owned();
    let user = scan.extr(" value=\"", "\"").to_owned();
    if token.is_empty() || user.is_empty() {
        return false;
    }
    let Ok(url) = http.api(&format!("{ROOT}/~account/warnings/accept")) else {
        return false;
    };
    let _ = http
        .post_form_text(
            &url,
            &[("_token", token.as_str()), ("user", user.as_str())],
            &[],
        )
        .await;
    true
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Toyhouse URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/~images/{id}"))?)
                .await?;
            let items = parse_post(&page, "<img class=\"mw-100\" src=\"", &Map::new())
                .into_iter()
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
        Target::Art(user) => crate::sites::paginate(Some((1u64, false)), move |state| {
            let user = user.clone();
            async move {
                let Some((page_num, warned)) = state else {
                    return Ok(None);
                };
                let mut url = http.api(&format!("{ROOT}/{user}/art"))?;
                url.query_pairs_mut()
                    .append_pair("page", &page_num.to_string());
                let page = http.text(&url).await?;
                let mut extra = Map::new();
                extra.insert("user".into(), user.as_str().into());
                let posts = extract_iter(&page, "<li class=\"gallery-item\">", "</li>");
                if posts.is_empty() && page_num == 1 && !warned && accept_warning(http, &page).await
                {
                    return Ok(Some((crate::sites::events(Vec::new()), Some((1, true)))));
                }
                let next = (posts.len() >= 18).then_some((page_num + 1, warned));
                let events = posts
                    .into_iter()
                    .filter_map(|post| parse_post(post, "<a href=\"", &extra))
                    .flat_map(|item| crate::sites::post(&SITE, http, &ART, vec![item]))
                    .collect();
                Ok(Some((crate::sites::events(events), next)))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.toyhou.se/d-floe/art"),
            Some(Target::Art("d-floe".into()))
        );
        assert_eq!(
            t(
                "https://f2.toyhou.se/file/f2-toyhou-se/watermarks/36817425_bqhGcwcnU.png?1625561467"
            ),
            Some(Target::Image("36817425".into()))
        );
    }

    #[tokio::test]
    async fn accepts_warning_then_lists_art() {
        let warning =
            r#"<form><input name="_token" value="TOK"><input name="user" value="5"></form>"#;
        let post = "<li class=\"gallery-item\"><a href=\"https://f2.toyhou.se/file/f2/images/123_abc.png\"></a></h2>\n            <div class=\"mb-1\">05 Mar 2021, 01:02:03 PM</div><div class=\"artist-credit\"><a>Artist</a></div>\n                    </div></li>";
        let (root, server) = serve(vec![html(warning), html("ok"), html(post)]);
        let http = client("toyhouse", root);
        let files = media(extract(
            &http,
            Url::parse("https://toyhou.se/jane/art").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(
            (k["id"].as_str(), k["hash"].as_str(), k["user"].as_str()),
            (Some("123"), Some("abc"), Some("jane"))
        );
        assert_eq!(k["date"], "2021-03-05 13:02:03");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("toyhouse/jane")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("POST /~account/warnings/accept ")
                && requests[1].ends_with("_token=TOK&user=5")
        );
    }
}
