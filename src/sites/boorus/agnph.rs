//! AGNPH (agn.ph), mirroring gallery_dl/extractor/agnph.py through the
//! XML API of the gallery.
use super::{booru_fields, date_value, item, parse_int, parse_query, per_post};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://agn.ph";
const COOKIE: &str = "confirmed_age=true";
/// `AgnphExtractor.page_start`.
const PAGE_START: i64 = 1;
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");

pub static AGNPH: Site = Site {
    category: "agnph",
    name: "AGNPH",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &POST],
    api_key: false,
};

#[derive(Debug, PartialEq)]
enum Target {
    Tag(Vec<(String, String)>),
    Post(String),
}

/// `(?:https?://)?agn\.ph/gallery/post/(?:\?([^#]+))?$` and
/// `(?:https?://)?agn\.ph/gallery/post/show/(\d+)`.
fn target(url: &Url) -> Option<Target> {
    let rest = super::below(url, &["agn.ph"])?;
    if let Some(tail) = rest.strip_prefix("/gallery/post/show/") {
        return super::leading_digits(tail).map(|id| Target::Post(id.into()));
    }
    if url.fragment().is_some() || url.path() != "/gallery/post/" {
        return None;
    }
    match url.query() {
        None => Some(Target::Tag(Vec::new())),
        Some("") => None,
        Some(query) => Some(Target::Tag(parse_query(query, false))),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Tag(params)) => {
            let mut data = Map::new();
            let search = super::query_get(&params, "search").unwrap_or_default();
            data.insert("search_tags".into(), search.into());
            paginate(http, params, data)
        }
        Some(Target::Post(id)) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/gallery/post/show/{id}/?api=xml"))?;
            let body = http.text_with(&url, &[("Cookie", COOKIE)]).await?;
            let doc = parse_xml(&body)?;
            let post = xml_to_dict(doc.root_element());
            let items = prepare(http, vec![post], &Map::new()).await?;
            Ok(crate::sites::events(per_post(&AGNPH, http, &POST, items)))
        }),
        None => crate::sites::fail(error("unsupported AGNPH URL")),
    }
}

fn parse_xml(body: &str) -> Result<roxmltree::Document<'_>> {
    roxmltree::Document::parse(body).map_err(|_| error("AGNPH returned invalid XML"))
}

/// `_xml_to_dict`: each child element's tag and text (`None` when empty).
fn xml_to_dict(node: roxmltree::Node<'_, '_>) -> Map<String, Value> {
    node.children()
        .filter(roxmltree::Node::is_element)
        .map(|child| {
            let text = child
                .first_child()
                .filter(roxmltree::Node::is_text)
                .and_then(|t| t.text())
                .map_or(Value::Null, |t| Value::String(t.to_owned()));
            (child.tag_name().name().to_owned(), text)
        })
        .collect()
}

/// Set `name` in an ordered parameter list, keeping its position.
fn set_param(params: &mut Vec<(String, String)>, name: &str, value: String) {
    match params.iter_mut().find(|(n, _)| n == name) {
        Some(pair) => pair.1 = value,
        None => params.push((name.into(), value)),
    }
}

/// `AgnphExtractor._pagination` over `/gallery/post/`.
fn paginate<'a>(
    http: &'a Http,
    mut params: Vec<(String, String)>,
    data: Map<String, Value>,
) -> Events<'a> {
    set_param(&mut params, "api", "xml".into());
    let page = match super::query_get(&params, "page") {
        Some(page) => PAGE_START + parse_int(page) - 1,
        None => PAGE_START,
    };
    crate::sites::paginate(Some(page), move |page| {
        let mut params = params.clone();
        let data = data.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            set_param(&mut params, "page", page.to_string());
            let mut url = http.api(&format!("{ROOT}/gallery/post/"))?;
            url.query_pairs_mut().extend_pairs(&params);
            let body = http.text_with(&url, &[("Cookie", COOKIE)]).await?;
            let (posts, done) = {
                let doc = parse_xml(&body)?;
                let root = doc.root_element();
                let posts: Vec<_> = root
                    .children()
                    .filter(roxmltree::Node::is_element)
                    .map(xml_to_dict)
                    .collect();
                let attr = |name| {
                    root.attribute(name)
                        .and_then(|v| v.trim().parse::<i64>().ok())
                };
                let done = match (attr("offset"), attr("count")) {
                    (Some(offset), Some(count)) => offset + posts.len() as i64 >= count,
                    _ => true,
                };
                (posts, done)
            };
            let items = prepare(http, posts, &data).await?;
            let events = per_post(&AGNPH, http, &TAG, items);
            let next = (!done).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// `BooruExtractor.items` with `AgnphExtractor._prepare` and, with the
/// `tags` option, `_tags` from the post page.
async fn prepare(
    http: &Http,
    posts: Vec<Map<String, Value>>,
    data: &Map<String, Value>,
) -> Result<Vec<Item>> {
    let tags = http.option_bool("tags", false);
    let mut items = Vec::new();
    for mut fields in posts {
        let Some(url) = fields
            .get("file_url")
            .and_then(Value::as_str)
            .and_then(|raw| super::file_url(ROOT, raw))
        else {
            continue;
        };
        if tags && let Some(id) = fields.get("id").and_then(Value::as_str) {
            let page = http.api(&format!("{ROOT}/gallery/post/show/{id}/"))?;
            let html = http.text_with(&page, &[("Cookie", COOKIE)]).await?;
            extended_tags(&mut fields, &html);
        }
        booru_fields(&url, &mut fields, data);
        let created = fields
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|ts| ts.trim().replace('_', "").parse::<i64>().ok());
        fields.insert("date".into(), date_value(created));
        if let Some(status) = fields.get("status").and_then(Value::as_str) {
            let status = status.trim().to_owned();
            fields.insert("status".into(), status.into());
        }
        let children = fields
            .get("has_children")
            .and_then(Value::as_str)
            .is_some_and(|v| v.contains("true"));
        fields.insert("has_children".into(), children.into());
        let mut item = item(url, fields, "creator", "tags", created);
        if item.url.host_str() == Some("agn.ph") {
            item = item.header("Cookie", COOKIE);
        }
        items.push(item);
    }
    Ok(items)
}

fn tag_type(code: char) -> Option<&'static str> {
    Some(match code {
        'a' => "artist",
        'b' => "copyright",
        'c' => "character",
        'd' => "species",
        'm' => "general",
        _ => return None,
    })
}

/// `AgnphExtractor._tags`: `class="(.)typetag">([^<]+)` in the tag list.
fn extended_tags(fields: &mut Map<String, Value>, page: &str) {
    let Some(container) = crate::sites::extr(page, "<ul class=\"taglist\">", "<h3>Statistics</h3>")
    else {
        return;
    };
    let mut tags: Vec<(char, Vec<String>)> = Vec::new();
    let marker = "typetag\">";
    let mut pos = 0;
    while let Some(found) = container[pos..].find(marker) {
        let at = pos + found;
        pos = at + marker.len();
        let Some(code) = container[..at].chars().next_back() else {
            continue;
        };
        if !container[..at - code.len_utf8()].ends_with("class=\"") {
            continue;
        }
        let name = super::until(&container[pos..], &['<']);
        if name.is_empty() {
            continue;
        }
        pos += name.len();
        let name = super::unquote(name).replace(' ', "_");
        match tags.iter_mut().find(|(c, _)| *c == code) {
            Some((_, list)) => list.push(name),
            None => tags.push((code, vec![name])),
        }
    }
    for (code, names) in tags {
        // Upstream raises KeyError for unknown tag types; skip them instead.
        if let Some(kind) = tag_type(code) {
            fields.insert(format!("tags_{kind}"), names.join(" ").into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, keywords, media, response, serve};
    use futures_util::StreamExt;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("https://agn.ph/gallery/post/?search=azuu"),
            Some(Target::Tag(vec![("search".into(), "azuu".into())]))
        );
        assert_eq!(t("https://agn.ph/gallery/post/"), Some(Target::Tag(vec![])));
        assert_eq!(
            t("http://agn.ph/gallery/post/show/501604/"),
            Some(Target::Post("501604".into()))
        );
        for raw in [
            "https://agn.ph/gallery/post",
            "https://agn.ph/gallery/post/?",
            "https://agn.ph/gallery/post/?search=a#x",
            "https://agn.ph/gallery/post/x",
            "https://agn.ph/gallery/post/show/x",
            "https://www.agn.ph/gallery/post/show/1",
            "https://agn.ph.evil.test/gallery/post/show/1",
            "https://agn.ph:8443/gallery/post/show/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn xml(body: &str) -> Vec<u8> {
        response("200 OK", "text/xml", body.as_bytes())
    }

    fn post_xml(id: u32) -> String {
        format!(
            "<post><id>{id}</id><created_at>1722041591</created_at><creator_id>-1</creator_id>\
             <md5>md5{id}</md5><file_url>http://agn.ph/gallery/data/7d/a5/md5{id}.png</file_url>\
             <status>approved </status><has_children>false</has_children>\
             <parent_id/><description></description><tags>a b</tags><height>1000</height></post>"
        )
    }

    #[tokio::test]
    async fn paginates_by_offset_and_count() {
        let first = format!(
            "<?xml version=\"1.0\"?><posts count=\"3\" offset=\"0\">{}{}</posts>",
            post_xml(1),
            post_xml(2)
        );
        let second = format!("<posts count=\"3\" offset=\"2\">{}</posts>", post_xml(3));
        let (root, server) = serve(vec![xml(&first), xml(&second)]);
        let http = http("agnph", root);
        let files = media(extract(
            &http,
            Url::parse("https://agn.ph/gallery/post/?search=a+b&page=2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let file = &files[0];
        assert_eq!(
            file.url.as_str(),
            "http://agn.ph/gallery/data/7d/a5/md51.png"
        );
        assert_eq!(file.headers["Cookie"], COOKIE);
        let k = keywords(file);
        assert_eq!(k["id"], "1");
        assert_eq!(k["height"], "1000");
        assert_eq!(k["search_tags"], "a b");
        assert_eq!(k["date"], "2024-07-27 00:53:11");
        assert_eq!(k["status"], "approved");
        assert_eq!(k["has_children"], false);
        assert_eq!(k["parent_id"], Value::Null);
        assert_eq!(k["description"], Value::Null);
        assert_eq!(k["filename"], "md51");
        assert_eq!(k["extension"], "png");
        assert_eq!(
            file.metadata.as_ref().unwrap().timestamp,
            Some(1_722_041_591)
        );
        let (name, dir) = crate::sites::default_templates(file).unwrap();
        assert_eq!(name.render(file).unwrap(), "agnph_1_md51.png");
        assert_eq!(
            dir.render(file).unwrap().unwrap(),
            std::path::PathBuf::from("agnph/a b")
        );
        assert_eq!(
            crate::sites::archive_key(file).unwrap(),
            ("agnph", r#"["t_{search_tags}_{id}",["a b","1"]]"#.into())
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /gallery/post/?search=a+b&page=2&api=xml "),
            "{}",
            requests[0]
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("cookie: confirmed_age=true")
        );
        assert!(requests[1].starts_with("GET /gallery/post/?search=a+b&page=3&api=xml "));
    }

    #[tokio::test]
    async fn post_with_extended_tags() {
        let page = "<ul class=\"taglist\"><li><a class=\"atypetag\">reyn goldfur</a></li>\
            <li><a class=\"dtypetag\">sneasel</a><a class=\"dtypetag\">regional%20form</a></li>\
            <li><a class=\"mtypetag\">solo</a></li></ul><h3>Statistics</h3>";
        let (root, server) = serve(vec![
            xml(&post_xml(501604)),
            response("200 OK", "text/html", page.as_bytes()),
        ]);
        let options = serde_json::json!({"tags": true});
        let http = http("agnph", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://agn.ph/gallery/post/show/501604/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "post");
        assert_eq!(k["tags_artist"], "reyn_goldfur");
        assert_eq!(k["tags_species"], "sneasel regional_form");
        assert_eq!(k["tags_general"], "solo");
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["501604"]]"#
        );
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("agnph")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/post/show/501604/?api=xml "));
        assert!(requests[1].starts_with("GET /gallery/post/show/501604/ "));
    }

    #[tokio::test]
    async fn reports_invalid_xml_and_missing_posts() {
        let (root, _server) = serve(vec![
            xml("<posts count=\"1\""),
            response("404 Not Found", "text/html", b""),
        ]);
        let http = http("agnph", root);
        let results: Vec<_> = extract(
            &http,
            Url::parse("https://agn.ph/gallery/post/?search=a").unwrap(),
        )
        .collect()
        .await;
        assert!(results.iter().any(Result::is_err));
        let results: Vec<_> = extract(
            &http,
            Url::parse("https://agn.ph/gallery/post/show/1/").unwrap(),
        )
        .collect()
        .await;
        assert!(results.iter().any(Result::is_err));
    }
}
