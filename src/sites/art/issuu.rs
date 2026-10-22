//! Issuu publications and publishers, mirroring gallery_dl/extractor/issuu.py
//! by reading the document data embedded in the publication pages.
use super::{extr, on_host, parse_int, raw_segments, segment, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://issuu.com";
static PUBLICATION: Route = Route::new(
    "publication",
    "{num:>03}.{extension}",
    &[
        "{category}",
        "{document[username]}",
        "{document[date]:%Y-%m-%d} {document[title]}",
    ],
    "{document[publicationId]}_{num}",
);
// Upstream `IssuuUserExtractor` inherits the `Extractor` defaults.
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "issuu",
    name: "Issuu",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&PUBLICATION, &USER],
    api_key: false,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// `/USER/docs/NAME`, as a raw path.
    Publication(String),
    User {
        user: String,
        page: i64,
    },
}

fn target(url: &Url) -> Option<Target> {
    // Upstream matches `issuu\.com` without a `www.` prefix.
    if !crate::sites::plain(url) || url.host_str() != Some("issuu.com") {
        return None;
    }
    let parts = raw_segments(url);
    if let [user, "docs", name, ..] = parts.as_slice() {
        return Some(Target::Publication(format!("/{user}/docs/{name}")));
    }
    // `/([^/?#]+)(?:/(\d*))?$`: no query, fragment or further segments.
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let rest = url.path().strip_prefix('/')?;
    let (user, page) = rest.split_once('/').unwrap_or((rest, ""));
    (segment(user) && page.bytes().all(|b| b.is_ascii_digit()) && page.len() <= 18).then(|| {
        Target::User {
            user: user.into(),
            page: parse_int(page, 1),
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Publication(path)) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}{path}"))?;
            let page = http.text(&url).await?;
            let items = parse_publication(&page)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &PUBLICATION,
                items,
            )))
        }),
        Some(Target::User { user, page }) => user_pages(http, user, page),
        None => crate::sites::fail(error("unsupported Issuu URL")),
    }
}

/// The publication's `document` and one file per page image.
pub(crate) fn parse_publication(page: &str) -> Result<Vec<Item>> {
    let data = extr(page, "{\\\"documentTextVersion\\\":", "]\\n\"])</script>");
    let data = crate::sites::unescape(&format!("{{\"\":{}", data.replace("\\\"", "\"")));
    let data = super::json_loads(&data, "Issuu")?;
    let Some(Value::Object(mut doc)) = data.pointer("/initialDocumentData/document").cloned()
    else {
        return Err(error("Issuu page contains no document data"));
    };
    let published = super::parse_iso(
        doc.get("originalPublishDateInISOString")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    // Nested datetimes need the internal source-date form to format in
    // templates; public JSON output renders it as date text.
    doc.insert("date".into(), crate::filter::source_date_json(published));
    let count = match doc.get("pageCount") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_default(),
        Some(Value::String(s)) => parse_int(s, 0),
        _ => 0,
    }
    .clamp(0, 10_000);
    let doc = Value::Object(doc);
    let base = format!(
        "https://image.isu.pub/{}-{}/jpg/page_",
        crate::sites::scalar(doc.get("revisionId").unwrap_or(&Value::Null)),
        crate::sites::scalar(doc.get("publicationId").unwrap_or(&Value::Null)),
    );
    let id = crate::sites::scalar(doc.get("publicationId").unwrap_or(&Value::Null));
    let user = str_of(&doc, "username").to_owned();
    let title = str_of(&doc, "title").to_owned();
    let mut items = Vec::new();
    for num in 1..=count {
        let url = super::url(&format!("{base}{num}.jpg"))?;
        let mut fields = Map::new();
        fields.insert("document".into(), doc.clone());
        fields.insert("count".into(), count.into());
        fields.insert("num".into(), num.into());
        super::nameext_into(url.as_str(), &mut fields);
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), format!("{id}_{num}"))
                .user(Some(user.clone()))
                .caption(Some(title.clone()))
                .timestamp(published),
        );
    }
    Ok(items)
}

/// The publications listed on one publisher page.
pub(crate) fn parse_docs(html: &str) -> Option<Vec<Value>> {
    let data = super::extr(html, "\\\"docs\\\":", "}]\\n\"]");
    serde_json::from_str::<Value>(&data.replace("\\\"", "\""))
        .ok()?
        .as_array()
        .cloned()
}

fn user_pages(http: &Http, user: String, page: i64) -> Events<'_> {
    crate::sites::paginate(Some(page), move |pnum| {
        let user = user.clone();
        async move {
            let Some(pnum) = pnum else {
                return Ok(None);
            };
            let base = format!("{ROOT}/{user}");
            let url = if pnum > 1 {
                format!("{base}/{pnum}")
            } else {
                base
            };
            let html = http.text(&http.api(&url)?).await?;
            // Upstream logs and stops when a page holds no document list.
            let Some(docs) = parse_docs(&html) else {
                return Ok(None);
            };
            let mut children = Vec::new();
            for publication in &docs {
                let Some(data) = publication.as_object() else {
                    continue;
                };
                let Ok(child) = Url::parse(&format!("{ROOT}/{}", str_of(publication, "uri")))
                else {
                    continue;
                };
                if matches!(target(&child), Some(Target::Publication(_))) {
                    children.push((child, data.clone()));
                }
            }
            let next = (docs.len() >= 48 && pnum < 10_000).then_some(pnum + 1);
            let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
                    crate::sites::queue(http, "issuu", "user", child, &data)
                })),
            ));
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://issuu.com/issuu/docs/motions-1-2019/"),
            Some(Target::Publication("/issuu/docs/motions-1-2019".into()))
        );
        assert_eq!(
            t("https://issuu.com/issuu"),
            Some(Target::User {
                user: "issuu".into(),
                page: 1
            })
        );
        assert_eq!(
            t("https://issuu.com/issuu/3"),
            Some(Target::User {
                user: "issuu".into(),
                page: 3
            })
        );
        for raw in [
            "https://issuu.com/",
            "https://issuu.com/issuu?x=1",
            "https://issuu.com/issuu/docs",
            "https://issuu.com/issuu/x",
            "https://www.issuu.com/issuu",
            "https://issuu.com.evil.com/issuu",
            "https://evilissuu.com/issuu",
            "https://issuu.com:8080/issuu",
            "https://u:p@issuu.com/issuu",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn page() -> String {
        concat!(
            r#"<script>self.__next_f.push([1,"x:{\"documentTextVersion\":1,"#,
            r#"\"initialDocumentData\":{\"document\":{\"username\":\"issuu\","#,
            r#"\"title\":\"A &amp; B\",\"publicationId\":\"pub\",\"revisionId\":\"rev\","#,
            r#"\"pageCount\":2,\"originalPublishDateInISOString\":\"2019-04-08T12:00:00.000Z\"}}}"#,
            r#"]\n"])</script>"#
        )
        .to_owned()
    }

    #[tokio::test]
    async fn extracts_publication_pages() {
        let (root, server) = serve(vec![html(&page())]);
        let http = http("issuu", root);
        let files = media(extract(
            &http,
            Url::parse("https://issuu.com/issuu/docs/motions").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://image.isu.pub/rev-pub/jpg/page_2.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["document"]["title"], "A & B");
        let public = crate::postprocess::public_keywords(&crate::postprocess::context(&files[0]));
        assert_eq!(public["document"]["date"], "2019-04-08 12:00:00");
        assert_eq!(k["count"], 2);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("issuu/issuu/2019-04-08 A & B")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["{document[publicationId]}_{num}",["pub","2"]]"#
        );
        assert!(server.join().unwrap()[0].starts_with("GET /issuu/docs/motions "));
    }

    #[tokio::test]
    async fn user_queues_publications() {
        let listing = r#"x{\"docs\":[{\"uri\":\"issuu/docs/motions\",\"title\":\"M\"}]}]\n"]"#;
        let (root, server) = serve(vec![html(listing), html(&page())]);
        let http = http("issuu", root);
        let files = media(extract(
            &http,
            Url::parse("https://issuu.com/issuu").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /issuu "));
        assert!(requests[1].starts_with("GET /issuu/docs/motions "));
    }
}
