//! cfake celebrity, category, created and country galleries, mirroring
//! gallery_dl/extractor/cfake.py.
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, group, nameext_into, parse_int, re, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://cfake.com";
const FILENAME: &str = "{category}_{type_name}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{type}", "{type_name} ({type_id})"];

static CELEBRITY: Route = Route::new("celebrity", FILENAME, DIRECTORY, "{id}");
static CATEGORY: Route = Route::new("category", FILENAME, DIRECTORY, "{id}");
static CREATED: Route = Route::new("created", FILENAME, DIRECTORY, "{id}");
static COUNTRY: Route = Route::new("country", FILENAME, DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "cfake",
    name: "cfake",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CELEBRITY, &CATEGORY, &CREATED, &COUNTRY],
    api_key: false,
    cookies: None,
};

/// Route, type, name, type ID, sub ID and page.
type Target = (&'static Route, String, String, String, String, String);

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?cfake\.com/images/";
    let simple = re!(&format!(
        r"{BASE}(celebrity|categories)/([^/?#]+)/(\d+)()(?:/p(\d+))?"
    ));
    let nested = re!(&format!(
        r"{BASE}(created|country)/([^/?#]+)/(\d+)/(\d+)(?:/p(\d+))?"
    ));
    let c = captures(simple, url).or_else(|| captures(nested, url))?;
    let route: &'static Route = match &c[1] {
        "celebrity" => &CELEBRITY,
        "categories" => &CATEGORY,
        "created" => &CREATED,
        _ => &COUNTRY,
    };
    Some((
        route,
        group(&c, 1).into(),
        group(&c, 2).into(),
        group(&c, 3).into(),
        group(&c, 4).into(),
        group(&c, 5).into(),
    ))
}

fn images(page: &str) -> Vec<(String, Map<String, Value>)> {
    extract_iter(page, "<a href=\"javascript:showimage(", "</div></div>")
        .into_iter()
        .filter_map(|item| {
            let show = extr(item, "show=", "&");
            if show.is_empty() {
                return None;
            }
            let picture = extr(item, "id_picture=", "&");
            let name = extr(item, "p_name=", "'");
            let rating = extr(
                extr(item, "class=\"current-rating\"", "</li>"),
                "width:",
                "px",
            );
            let url = format!("{ROOT}/medias/photos/{show}");
            let mut data = Map::new();
            data.insert("url".into(), url.as_str().into());
            data.insert("id".into(), parse_int(picture).into());
            data.insert("name".into(), unescape(name).into());
            data.insert(
                "date".into(),
                extr(item, "id=\"date_vignette\">", "</div>").into(),
            );
            data.insert("rating".into(), rating.into());
            Some((url, data))
        })
        .collect()
}

/// `_check_pagination`: the next page number if the page links to it.
fn next_page(page: &str) -> Option<i64> {
    let current = extr(page, "id=\"num_page_current\"", "</div>");
    let number = parse_int(extr(current, "\">", "</a>"));
    if number == 0 {
        return None;
    }
    let next = number + 1;
    (page.contains(&format!("/p{next}\"")) || page.contains(&format!("/p{next} "))).then_some(next)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((route, kind, name, type_id, sub_id, page)) = target(&url) else {
        return crate::sites::fail(error("unsupported cfake URL"));
    };
    let kind = match kind.strip_suffix("ies") {
        Some(stem) => format!("{stem}y"),
        None => kind,
    };
    let mut base = format!("{ROOT}/images/{kind}/{name}/{type_id}");
    if !sub_id.is_empty() {
        base = format!("{base}/{sub_id}");
    }
    let mut data = Map::new();
    data.insert("type".into(), kind.as_str().into());
    data.insert("type_id".into(), parse_int(&type_id).into());
    data.insert("type_name".into(), unquote(&name).replace('_', " ").into());
    data.insert("sub_id".into(), parse_int(&sub_id).into());
    let first = crate::sites::art::parse_int(&page, 1);
    crate::sites::paginate(Some(first), move |state| {
        let base = base.clone();
        let mut data = data.clone();
        async move {
            let Some(number) = state else { return Ok(None) };
            let url = if number < 2 {
                base.clone()
            } else {
                format!("{base}/p{number}")
            };
            let page = http.text(&http.api(&url)?).await?;
            data.insert("page".into(), number.into());
            let found = images(&page);
            let items: Vec<Item> = found
                .iter()
                .enumerate()
                .filter_map(|(index, (raw, image))| {
                    let mut fields = data.clone();
                    fields.extend(image.clone());
                    fields.insert("num".into(), (index as i64 + 1 + (number - 1) * 50).into());
                    nameext_into(raw, &mut fields);
                    let id = fields
                        .get("id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default();
                    Some(Item::new(Url::parse(raw).ok()?, fields).ids(id.clone(), id))
                })
                .collect();
            let next = if found.is_empty() {
                None
            } else {
                next_page(&page)
            };
            let events = items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t =
            |raw: &str| target(&Url::parse(raw).unwrap()).map(|t| (t.0.subcategory, t.1, t.4, t.5));
        assert_eq!(
            t("https://cfake.com/images/celebrity/Kaley_Cuoco/631/p2"),
            Some(("celebrity", "celebrity".into(), String::new(), "2".into()))
        );
        assert_eq!(
            t("https://cfake.com/images/created/Brooklyn_Nine-Nine/4142/4/p2"),
            Some(("created", "created".into(), "4".into(), "2".into()))
        );
        assert_eq!(
            t("https://cfake.com/images/categories/Facial/25/"),
            Some((
                "category",
                "categories".into(),
                String::new(),
                String::new()
            ))
        );
    }

    #[tokio::test]
    async fn pages_gallery() {
        let item = |id: u32| {
            format!(
                r#"<a href="javascript:showimage('big.php?show=2025/img{id}.jpg&id_picture={id}&p_name=Kaley &amp; Co')"><div id="date_vignette">2025-01-01</div><li class="current-rating" style="width:80px"></li></div></div>"#
            )
        };
        let first = format!(
            r#"{}<div id="num_page_current" ><a href="/images/celebrity/Kaley_Cuoco/631/p1">1</a></div><a href="/images/celebrity/Kaley_Cuoco/631/p2">"#,
            item(1)
        );
        let second = format!(
            r#"{}<div id="num_page_current" ><a href="x">2</a></div>"#,
            item(2)
        );
        let (root, server) = serve(vec![html(&first), html(&second)]);
        let http = client("cfake", root);
        let files = media(extract(
            &http,
            Url::parse("https://cfake.com/images/celebrity/Kaley_Cuoco/631/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://cfake.com/medias/photos/2025/img2.jpg"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 51);
        assert_eq!(k["name"], "Kaley & Co");
        assert_eq!(k["type_name"], "Kaley Cuoco");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "cfake_Kaley Cuoco_2.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("cfake/celebrity/Kaley Cuoco (631)")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /images/celebrity/Kaley_Cuoco/631/p2 "));
    }
}
