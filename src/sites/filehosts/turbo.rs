//! turbo.cr albums and media, mirroring gallery_dl/extractor/turbo.py (a
//! lolisafe derivative): download URLs come from the signed `api/sign`
//! endpoint and need the page as `Referer`.
use super::Extr;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "turbo",
    name: "Turbo",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &MEDIA],
    api_key: false,
};

static ALBUM: Route = Route::new(
    "album",
    "{filename}.{extension}",
    &["{category}", "{album_name} ({album_id})"],
    "{album_id}_{id}",
);
static MEDIA: Route = Route::new(
    "media",
    "{filename}.{extension}",
    &["{category}"],
    "{album_id}_{id}",
);

const ROOT: &str = "https://turbo.cr";

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    Media(String),
}

/// `(?:www\.)?turbo(?:vid)?\.cr` or `saint\d*\.(?:su|pk|cr|to)`.
fn host_ok(host: &str) -> bool {
    let bare = host.strip_prefix("www.").unwrap_or(host);
    if bare == "turbo.cr" || bare == "turbovid.cr" {
        return true;
    }
    let Some((name, tld)) = host.split_once('.') else {
        return false;
    };
    name.strip_prefix("saint")
        .is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit()))
        && matches!(tld, "su" | "pk" | "cr" | "to")
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !host_ok(url.host_str()?) {
        return None;
    }
    match crate::sites::segments(url).as_slice() {
        [a, id, ..] if a == "a" => Some(Target::Album(id.clone())),
        [kind, id, ..] if matches!(kind.as_str(), "d" | "v" | "embed" | "embev") => {
            Some(Target::Media(id.clone()))
        }
        _ => None,
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(id)) => crate::sites::once(async move { album(http, &id).await }),
        Some(Target::Media(id)) => crate::sites::once(async move {
            let item = media(http, &id).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &MEDIA,
                vec![item],
            )))
        }),
        None => crate::sites::fail(error("unsupported turbo URL")),
    }
}

fn after_tag(text: &str) -> &str {
    text.find('>').map_or(text, |i| &text[i + 1..])
}

struct Row {
    id: String,
    size: i64,
}

fn parse_album(page: &str, album_id: &str) -> (Vec<Row>, Map<String, Value>) {
    let mut extr = Extr::new(page);
    let title = extr.next("<h1 ", "<");
    let descr = extr.next("<p ", "<");
    let tbody = extr.next("id=\"fileTbody\"", "</tbody>");
    let rows: Vec<Row> = super::extract_iter(tbody, "<tr", "</tr>")
        .into_iter()
        .map(|row| Row {
            id: crate::sites::extr(row, "data-id=\"", "\"")
                .unwrap_or_default()
                .to_owned(),
            size: super::parse_int(
                crate::sites::extr(row, "data-size=\"", "\"").unwrap_or_default(),
            ),
        })
        .collect();
    let mut data = Map::new();
    data.insert("album_id".into(), album_id.into());
    data.insert(
        "album_name".into(),
        crate::sites::unescape(after_tag(title)).into(),
    );
    data.insert(
        "description".into(),
        crate::sites::unescape(after_tag(descr)).into(),
    );
    let size: i64 = super::extract_iter(tbody, "data-size=\"", "\"")
        .into_iter()
        .map(super::parse_int)
        .sum();
    data.insert("album_size".into(), size.into());
    data.insert("count".into(), tbody.matches("data-id=").count().into());
    (rows, data)
}

/// The signed download URL and original file name.
async fn sign(http: &Http, id: &str, referer: &str) -> Result<(Url, String)> {
    if !valid_id(id) {
        return Err(error("invalid turbo file ID"));
    }
    let mut url = http.api(&format!("{ROOT}/api/sign"))?;
    url.query_pairs_mut().append_pair("v", id);
    let data = http.json_with(&url, &[("Referer", referer)]).await?;
    let name = data
        .get("original_filename")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| data.get("filename").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let file_url = data
        .get("url")
        .and_then(Value::as_str)
        .and_then(super::web_url)
        .ok_or_else(|| error("turbo returned an invalid file URL"))?;
    Ok((file_url, name))
}

/// The upstream file dictionary before album fields are merged.
fn file_fields(
    id: &str,
    url: &Url,
    size: i64,
    date: Option<Value>,
    name: &str,
) -> Map<String, Value> {
    let mut file = Map::new();
    file.insert("id".into(), id.into());
    file.insert("file".into(), url.as_str().into());
    file.insert("size".into(), size.into());
    if let Some(date) = date {
        file.insert("date".into(), date);
    }
    super::nameext_from_name(name, &mut file);
    file
}

/// Lolisafe naming: `{name}-{id}` unless the name already is the ID.
fn finish(
    url: Url,
    mut file: Map<String, Value>,
    data: &Map<String, Value>,
    num: usize,
    referer: &str,
    timestamp: Option<i64>,
) -> Item {
    for (key, value) in data {
        file.insert(key.clone(), value.clone());
    }
    file.insert("num".into(), num.into());
    let id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
    let filename = file
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    file.insert("name".into(), filename.clone().into());
    if filename != id {
        file.insert("filename".into(), format!("{filename}-{id}").into());
    }
    let album = data
        .get("album_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Item::new(url, file)
        .ids(album, id)
        .caption(Some(filename))
        .timestamp(timestamp)
        .header("Referer", referer)
}

async fn album<'a>(http: &'a Http, id: &str) -> Result<Events<'a>> {
    let referer = format!("{ROOT}/a/{id}");
    let page = http.text(&http.api(&referer)?).await?;
    let (rows, data) = parse_album(&page, id);
    let inputs: Vec<(usize, Row)> = rows.into_iter().enumerate().collect();
    Ok(super::each(
        &SITE,
        http,
        &ALBUM,
        inputs,
        move |(index, row)| {
            let data = data.clone();
            let referer = referer.clone();
            async move {
                let (url, name) = sign(http, &row.id, &referer).await?;
                let file = file_fields(&row.id, &url, row.size, None, &name);
                Ok(Some(finish(url, file, &data, index + 1, &referer, None)))
            }
        },
    ))
}

async fn media(http: &Http, id: &str) -> Result<Item> {
    if !valid_id(id) {
        return Err(error("invalid turbo file ID"));
    }
    let referer = format!("{ROOT}/d/{id}");
    let page = http.text(&http.api(&referer)?).await?;
    let size = crate::sites::extr(&page, "id=\"fileSizeBytes\">", "<").unwrap_or_default();
    let timestamp = page
        .find("File ID:")
        .and_then(|pos| super::extract(&page, "<span>", "<", pos))
        .and_then(|(date, _)| super::parse_iso(date));
    let (url, name) = sign(http, id, &referer).await?;
    let size = super::parse_float(&size.replace("&#43;", "+")) as i64;
    let file = file_fields(id, &url, size, Some(super::date_value(timestamp)), &name);
    let mut data = Map::new();
    data.insert("album_id".into(), "".into());
    data.insert("album_name".into(), "".into());
    data.insert("album_size".into(), (-1).into());
    data.insert("description".into(), "".into());
    data.insert("count".into(), 1.into());
    Ok(finish(url, file, &data, 1, &referer, timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media as collect, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://turbo.cr/a/AbC"),
            Some(Target::Album("AbC".into()))
        );
        assert_eq!(
            t("https://saint2.su/embed/xyz"),
            Some(Target::Media("xyz".into()))
        );
        assert_eq!(
            t("https://www.turbovid.cr/d/xyz"),
            Some(Target::Media("xyz".into()))
        );
        assert_eq!(
            t("https://saint.to/v/xyz"),
            Some(Target::Media("xyz".into()))
        );
        for raw in [
            "https://turbo.com/a/x",
            "https://www.saint2.su/a/x",
            "https://saintx.su/a/x",
            "https://turbo.cr.evil.test/a/x",
            "https://turbo.cr/embedx/x",
            "https://turbo.cr:8080/a/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = "<h1 class=\"t\">Holiday &amp; more</h1><p class=\"d\">Desc</p>\
<table><tbody id=\"fileTbody\"><tr data-id=\"f1\" data-size=\"100\"><td></td></tr>\
<tr data-id=\"f2\" data-size=\"50\"></tr></tbody></table>";

    #[tokio::test]
    async fn extracts_album() {
        let (root, server) = serve(vec![
            html(PAGE),
            json(json!({"url": "https://cdn.turbo.cr/f1?sig=1", "original_filename": "Pic.JPG"})),
            json(json!({"url": "https://cdn.turbo.cr/f2", "filename": "f2"})),
        ]);
        let http = http("Turbo", root);
        let files = collect(extract(
            &http,
            Url::parse("https://turbo.cr/a/ALB").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["album_name"], "Holiday & more");
        assert_eq!(k["description"], "Desc");
        assert_eq!(k["album_size"], 150);
        assert_eq!(k["count"], 2);
        assert_eq!(k["size"], 100);
        assert_eq!(k["name"], "Pic");
        assert_eq!(k["filename"], "Pic-f1");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(keywords(&files[1])["filename"], "f2");
        assert_eq!(
            files[0].headers.get("Referer").map(String::as_str),
            Some("https://turbo.cr/a/ALB")
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Pic-f1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("turbo/Holiday & more (ALB)")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{album_id}_{id}",["ALB","f1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/ALB "));
        assert!(requests[1].starts_with("GET /api/sign?v=f1 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("referer: https://turbo.cr/a/alb")
        );
    }

    #[tokio::test]
    async fn extracts_media() {
        let (root, server) = serve(vec![
            html(
                "<b id=\"fileSizeBytes\">1.5e&#43;3</b> File ID: x <span>2024-06-05T00:00:00Z</span>",
            ),
            json(json!({"url": "https://cdn.turbo.cr/m", "original_filename": "clip.mp4"})),
        ]);
        let http = http("Turbo", root);
        let files = collect(extract(
            &http,
            Url::parse("https://saint2.su/embed/MID").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "media");
        assert_eq!(k["size"], 1500);
        assert_eq!(k["date"], "2024-06-05 00:00:00");
        assert_eq!(k["album_size"], -1);
        assert_eq!(k["filename"], "clip-MID");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /d/MID "));
        assert!(requests[1].starts_with("GET /api/sign?v=MID "));
    }

    #[tokio::test]
    async fn rejects_invalid_signed_url() {
        let (root, _server) = serve(vec![
            html("<b id=\"fileSizeBytes\">1</b>"),
            json(json!({"url": "javascript:alert(1)", "filename": "x"})),
        ]);
        let http = http("Turbo", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://turbo.cr/d/MID").unwrap(),
        ))
        .await;
        assert!(
            events[0]
                .as_ref()
                .err()
                .unwrap()
                .to_string()
                .contains("invalid file URL")
        );
    }
}
