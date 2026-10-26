//! Lensdump albums, user album lists and images, mirroring
//! gallery_dl/extractor/lensdump.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, int_of, nameext_into, parse_int, parse_iso,
    parse_query, re, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://lensdump.com";

static ALBUM: Route = Route::new(
    "album",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);
static ALBUMS: Route = Route::new("albums", "{filename}.{extension}", &["{category}"], "");
static IMAGE: Route = Route::new(
    "image",
    "{category}_{id}{title:?_//}.{extension}",
    &["{category}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "lensdump",
    name: "Lensdump",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &ALBUMS, &IMAGE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String, String),
    Albums(String, String),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://lensdump\.com/a/(\w+)(?:/?\?([^#]+))?"), url) {
        return Some(Target::Album(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(
        re!(r"^https?://lensdump\.com/([^/?#]+)(?:/?\?([^#]+))?"),
        url,
    ) {
        let user = group(&c, 1);
        let rest = &url.path()[1..];
        if !(rest.starts_with("a/") || rest.starts_with("i/")) {
            return Some(Target::Albums(user.into(), group(&c, 2).into()));
        }
    }
    let image = re!(r"^https?://(?:(?:i\d?\.)?lensdump\.com|\w\.l3n\.co)/(?:i/)?(\w+)");
    captures(image, url).map(|c| Target::Image(group(&c, 1).into()))
}

/// `_pagination`: items between `begin` and `end` on every page.
async fn paginate(
    http: &Http,
    mut page: String,
    begin: &str,
    end: &str,
) -> crate::Result<Vec<String>> {
    let mut items = Vec::new();
    loop {
        items.extend(
            extract_iter(&page, begin, end)
                .into_iter()
                .map(str::to_owned),
        );
        let next = extr(&page, " data-pagination=\"next\"", ">");
        if next.is_empty() {
            return Ok(items);
        }
        let url = Url::parse(ROOT)
            .and_then(|root| root.join(extr(next, "href=\"", "\"")))
            .map_err(|_| error("invalid Lensdump page URL"))?;
        page = http.text(&http.api(url.as_str())?).await?;
    }
}

fn album_image(raw: &str) -> Option<(String, Map<String, Value>)> {
    let object = match extr(raw, "data-object='", "'") {
        "" => extr(raw, "data-object=\"", "\""),
        object => object,
    };
    let data: Value = serde_json::from_str(&unquote(object)).ok()?;
    let url = crate::sites::gdl::str_of(&data, "url").to_owned();
    let mut fields = Map::new();
    fields.insert(
        "id".into(),
        data.get("name").cloned().unwrap_or(Value::Null),
    );
    fields.insert("url".into(), url.as_str().into());
    let title = data
        .get("title")
        .and_then(Value::as_str)
        .map(|t| Value::from(unescape(t)));
    fields.insert("title".into(), title.unwrap_or(Value::Null));
    fields.insert(
        "name".into(),
        data.get("filename").cloned().unwrap_or(Value::Null),
    );
    fields.insert(
        "filename".into(),
        data.get("name").cloned().unwrap_or(Value::Null),
    );
    fields.insert(
        "extension".into(),
        data.get("extension").cloned().unwrap_or(Value::Null),
    );
    fields.insert("width".into(), int_of(data.get("width")).into());
    fields.insert("height".into(), int_of(data.get("height")).into());
    Some((url, fields))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Lensdump URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Album(id, query) => {
                let url = if query.is_empty() {
                    format!("{ROOT}/a/{id}")
                } else {
                    format!("{ROOT}/a/{id}/?{query}")
                };
                let page = http.text(&http.api(&url)?).await?;
                let title = unescape(extr(&page, "property=\"og:title\" content=\"", "\"").trim());
                let images: Vec<_> = paginate(http, page, " class=\"list-item ", ">")
                    .await?
                    .iter()
                    .filter_map(|raw| album_image(raw))
                    .collect();
                let count = images.len();
                let items = images
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, (raw, mut fields))| {
                        fields.insert("gallery_id".into(), id.as_str().into());
                        fields.insert("title".into(), title.as_str().into());
                        fields.insert("count".into(), count.into());
                        fields.insert("num".into(), (index + 1).into());
                        Some(
                            Item::new(Url::parse(&raw).ok()?, fields)
                                .ids(id.clone(), (index + 1).to_string()),
                        )
                    })
                    .collect();
                crate::sites::events(crate::sites::post(&SITE, http, &ALBUM, items))
            }
            Target::Albums(user, query) => {
                let mut url = http.api(&format!("{ROOT}/{user}/"))?;
                if query.is_empty() {
                    url.query_pairs_mut()
                        .append_pair("sort", "date_asc")
                        .append_pair("page", "1");
                } else {
                    let params = parse_query(&query);
                    url.query_pairs_mut().extend_pairs(
                        params
                            .iter()
                            .map(|(k, v)| (k.clone(), crate::sites::scalar(v))),
                    );
                }
                let page = http.text(&url).await?;
                let albums = paginate(http, page, "data-url-short=\"", "\"")
                    .await?
                    .into_iter()
                    .filter_map(|path| Url::parse(ROOT).ok()?.join(&path).ok())
                    .map(|url| (url, Map::new()))
                    .collect::<Vec<_>>();
                futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                    futures_util::stream::iter(albums.into_iter().map(move |(url, data)| {
                        crate::sites::queue(http, SITE.category, ALBUMS.subcategory, url, &data)
                    })),
                ))
            }
            Target::Image(key) => {
                let page = http.text(&http.api(&format!("{ROOT}/i/{key}"))?).await?;
                let mut scan = Scan::new(&page);
                let mut data = Map::new();
                data.insert("id".into(), key.as_str().into());
                data.insert(
                    "title".into(),
                    unescape(scan.extr("property=\"og:title\" content=\"", "\"")).into(),
                );
                let raw = scan
                    .extr("property=\"og:image\" content=\"", "\"")
                    .to_owned();
                data.insert("url".into(), raw.as_str().into());
                data.insert(
                    "width".into(),
                    parse_int(scan.extr("property=\"image:width\" content=\"", "\"")).into(),
                );
                data.insert(
                    "height".into(),
                    parse_int(scan.extr("property=\"image:height\" content=\"", "\"")).into(),
                );
                data.insert(
                    "date".into(),
                    date(parse_iso(scan.extr("<span title=\"", "\""))),
                );
                nameext_into(&raw, &mut data);
                let file = Url::parse(&raw).map_err(|_| error("Lensdump image has no URL"))?;
                crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &IMAGE,
                    vec![Item::new(file, data).ids(key.clone(), key)],
                ))
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://lensdump.com/a/1IhJr"),
            Some(Target::Album("1IhJr".into(), String::new()))
        );
        assert_eq!(
            t("https://lensdump.com/vstar925/?sort=likes_desc&page=1"),
            Some(Target::Albums(
                "vstar925".into(),
                "sort=likes_desc&page=1".into()
            ))
        );
        assert_eq!(
            t("https://lensdump.com/vstar925/albums"),
            Some(Target::Albums("vstar925".into(), String::new()))
        );
        assert_eq!(
            t("https://lensdump.com/i/tyoAyM"),
            Some(Target::Image("tyoAyM".into()))
        );
        assert_eq!(
            t("https://c.l3n.co/i/tyoAyM.webp"),
            Some(Target::Image("tyoAyM".into()))
        );
        assert_eq!(
            t("https://i3.lensdump.com/i/tyoAyM"),
            Some(Target::Image("tyoAyM".into()))
        );
    }

    #[tokio::test]
    async fn extracts_album_pages() {
        let object = |name: &str| {
            format!(
                "{{%22name%22:%22{name}%22,%22url%22:%22https://i.lensdump.com/i/{name}.jpg%22,%22title%22:%22T%20%26amp%3B%20U%22,%22filename%22:%22{name}.jpg%22,%22extension%22:%22jpg%22,%22width%22:%22800%22,%22height%22:600}}"
            )
        };
        let first = format!(
            r#"<meta property="og:title" content=" Album "><div class="list-item x" data-object='{}'>
            <a data-pagination="next" href="/a/1IhJr/?page=2">"#,
            object("aa")
        );
        let second = format!(
            r#"<div class="list-item y" data-object="{}">"#,
            object("bb")
        );
        let (root, server) = serve(vec![html(&first), html(&second)]);
        let http = client("lensdump", root);
        let files = media(extract(
            &http,
            Url::parse("https://lensdump.com/a/1IhJr").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["id"], "bb");
        assert_eq!(k["name"], "bb.jpg");
        assert_eq!(k["width"], 800);
        let (file, _) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "lensdump_1IhJr_002.jpg");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /a/1IhJr/?page=2 "));
    }

    #[tokio::test]
    async fn queues_user_albums() {
        let (root, server) = serve(vec![html(
            r#"<a data-url-short="https://lensdump.com/a/x1"></a><a data-url-short="/a/x2">"#,
        )]);
        let http = client("lensdump", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://lensdump.com/vstar925").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /vstar925/?sort=date_asc&page=1 "));
    }
}
