//! WikiArt artists, paintings and collections, mirroring
//! gallery_dl/extractor/wikiart.py through the site's `json=2` endpoints.
use super::{file_url, nameext_into, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.wikiart.org";

pub static SITE: Site = Site {
    category: "wikiart",
    name: "WikiArt",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ARTIST, &IMAGE, &ARTWORKS, &ARTISTS],
    api_key: false,
    cookies: None,
};

const FILENAME: &str = "{id}_{title}.{extension}";
static ARTIST: Route = Route::new(
    "artist",
    FILENAME,
    &["{category}", "{artist[artistName]}"],
    "{id}",
);
static IMAGE: Route = Route::new(
    "image",
    FILENAME,
    &["{category}", "{artist[artistName]}"],
    "{id}",
);
static ARTWORKS: Route = Route::new(
    "artworks",
    FILENAME,
    &["{category}", "Artworks by {group!c}", "{type}"],
    "{id}",
);
static ARTISTS: Route = Route::new("artists", FILENAME, &["{category}"], "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    Artist {
        lang: String,
        artist: String,
    },
    Image {
        lang: String,
        artist: String,
        title: String,
    },
    Artworks {
        lang: String,
        group: String,
        kind: String,
    },
    Artists {
        lang: String,
        group: String,
        kind: String,
    },
}

/// Python `[\w-]`.
fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

fn words(text: &str) -> bool {
    !text.is_empty() && text.chars().all(word)
}

/// The leading `[\w-]+` run of `text`.
fn leading_words(text: &str) -> Option<&str> {
    let end = text.find(|c| !word(c)).unwrap_or(text.len());
    (end > 0).then(|| &text[..end])
}

fn target(url: &Url) -> Option<Target> {
    if !super::on_host(url, &["wikiart.org"]) {
        return None;
    }
    let parts: Vec<&str> = url.path().split('/').skip(1).collect();
    let (lang, rest) = parts.split_first()?;
    if lang.is_empty() || !lang.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let lang = (*lang).to_owned();
    // Artist: `/(?!\w+-by-)([\w-]+)/?$`
    if let [artist] | [artist, ""] = rest {
        let run = artist
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(artist.len());
        if words(artist)
            && !(run > 0 && artist[run..].starts_with("-by-"))
            && url.query().is_none()
            && url.fragment().is_none()
        {
            return Some(Target::Artist {
                lang,
                artist: (*artist).to_owned(),
            });
        }
    }
    let [first, second, ..] = rest else {
        return None;
    };
    let second = leading_words(second)?.to_owned();
    if let Some(group) = first.strip_prefix("paintings-by-") {
        return words(group).then(|| Target::Artworks {
            lang,
            group: group.to_owned(),
            kind: second,
        });
    }
    if let Some(group) = first.strip_prefix("artists-by-") {
        return words(group).then(|| Target::Artists {
            lang,
            group: group.to_owned(),
            kind: second,
        });
    }
    words(first).then(|| Target::Image {
        lang,
        artist: (*first).to_owned(),
        title: second,
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported WikiArt URL"));
    };
    match target {
        Target::Artist { lang, artist } => crate::sites::once(async move {
            let data = artist_metadata(http, &lang, &artist).await?;
            let url = format!("{ROOT}/{lang}/{artist}/mode/all-paintings");
            Ok(paintings(http, &ARTIST, url, data, false))
        }),
        Target::Image {
            lang,
            artist,
            title,
        } => crate::sites::once(async move {
            let data = artist_metadata(http, &lang, &artist).await?;
            let name = match str_of(&data["artist"], "artistName") {
                "" => artist.as_str(),
                name => name,
            };
            let name = name.replace('%', "%25").replace(['?', '#'], "");
            let url = format!("{ROOT}/{lang}/Search/{name} {}", search_title(&title));
            Ok(paintings(http, &IMAGE, url, data, true))
        }),
        Target::Artworks { lang, group, kind } => {
            let url = format!("{ROOT}/{lang}/paintings-by-{group}/{kind}");
            let mut data = Map::new();
            data.insert("group".into(), group.into());
            data.insert("type".into(), kind.into());
            paintings(http, &ARTWORKS, url, data, false)
        }
        Target::Artists { lang, group, kind } => {
            let url = format!("{ROOT}/{lang}/App/Search/Artists-by-{group}");
            artists(http, url, kind)
        }
    }
}

/// Upstream image `paintings`: drop a trailing `-YEAR` from the slug.
fn search_title(title: &str) -> &str {
    match title.rsplit_once('-') {
        Some((head, year)) if !year.is_empty() && year.chars().all(char::is_numeric) => head,
        _ => title,
    }
}

async fn artist_metadata(http: &Http, lang: &str, artist: &str) -> Result<Map<String, Value>> {
    let url = http.api(&format!("{ROOT}/{lang}/{artist}?json=2"))?;
    let artist = http.json(&url).await?;
    if !artist.is_object() {
        return Err(error("WikiArt artist response is not an object"));
    }
    let mut data = Map::new();
    data.insert("artist".into(), artist);
    Ok(data)
}

/// Upstream `_pagination`: pages until `key` is empty, or one with `stop`.
fn pagination<'a>(
    http: &'a Http,
    url: String,
    extra: Vec<(&'static str, String)>,
    key: &'static str,
    stop: bool,
) -> impl futures_util::Stream<Item = Result<Vec<Value>>> + Send + 'a {
    stream::try_unfold(Some(1u64), move |page| {
        let url = url.clone();
        let extra = extra.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut params: Vec<(&str, String)> = vec![
                ("json", "2".into()),
                ("layout", "new".into()),
                ("page", page.to_string()),
                ("resultType", "masonry".into()),
            ];
            for (k, v) in extra {
                match params.iter_mut().find(|(key, _)| *key == k) {
                    Some(slot) => slot.1 = v,
                    None => params.push((k, v)),
                }
            }
            let mut request = http.api(&url)?;
            request.query_pairs_mut().extend_pairs(&params);
            let headers = [
                ("X-Requested-With", "XMLHttpRequest"),
                ("Referer", url.as_str()),
            ];
            let data = http.json_with(&request, &headers).await?;
            let items = match data.get(key) {
                Some(Value::Array(items)) if !items.is_empty() => items.clone(),
                _ => return Ok(None),
            };
            let next = (!stop).then_some(page + 1);
            Ok(Some((items, next)))
        }
    })
}

fn paintings<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    data: Map<String, Value>,
    stop: bool,
) -> Events<'a> {
    pagination(http, url, Vec::new(), "Paintings", stop)
        .map(move |page| {
            page.map(|paintings| {
                let items = paintings
                    .iter()
                    .filter_map(|painting| parse_painting(painting, &data))
                    .collect();
                crate::sites::events(crate::sites::post(&SITE, http, route, items))
            })
        })
        .map(|page| page.unwrap_or_else(crate::sites::fail))
        .flatten()
        .boxed()
}

pub(crate) fn parse_painting(painting: &Value, data: &Map<String, Value>) -> Option<Item> {
    let mut fields = painting.as_object()?.clone();
    let raw = str_of(painting, "image");
    let url = file_url(raw, ROOT)?;
    super::update(&mut fields, data);
    nameext_into(raw, &mut fields);
    let id = fields
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = fields
        .get("artistName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = str_of(painting, "title").to_owned();
    Some(
        Item::new(url, fields)
            .ids(id.clone(), id)
            .user(user)
            .caption(Some(title)),
    )
}

/// Upstream artists `items`: queue each artist page.
fn artists(http: &Http, url: String, kind: String) -> Events<'_> {
    let extra = vec![("json", "3".to_owned()), ("searchterm", kind)];
    pagination(http, url, extra, "Artists", false)
        .map(move |page| match page {
            Ok(artists) => stream::iter(artists.into_iter().filter_map(|artist| {
                let path = str_of(&artist, "artistUrl");
                let url = Url::parse(&format!("{ROOT}{path}")).ok()?;
                if !path.starts_with('/') || target(&url).is_none() {
                    return None;
                }
                Some((url, artist.as_object()?.clone()))
            }))
            .map(move |(url, data)| crate::sites::queue(http, "wikiart", "artists", url, &data))
            .flatten()
            .boxed(),
            Err(e) => crate::sites::fail(e),
        })
        .flatten()
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.wikiart.org/en/thomas-cole"),
            Some(Target::Artist {
                lang: "en".into(),
                artist: "thomas-cole".into()
            })
        );
        assert_eq!(
            t("https://wikiart.org/de/thomas-cole/"),
            Some(Target::Artist {
                lang: "de".into(),
                artist: "thomas-cole".into()
            })
        );
        assert_eq!(
            t("https://www.wikiart.org/en/thomas-cole/the-departure-1838"),
            Some(Target::Image {
                lang: "en".into(),
                artist: "thomas-cole".into(),
                title: "the-departure-1838".into()
            })
        );
        assert_eq!(
            t("https://www.wikiart.org/en/paintings-by-media/grisaille"),
            Some(Target::Artworks {
                lang: "en".into(),
                group: "media".into(),
                kind: "grisaille".into()
            })
        );
        assert_eq!(
            t("https://www.wikiart.org/en/artists-by-art-movement/academicism?x"),
            Some(Target::Artists {
                lang: "en".into(),
                group: "art-movement".into(),
                kind: "academicism".into()
            })
        );
        for raw in [
            "https://www.wikiart.org/en/paintings-by-media",
            "https://www.wikiart.org/en/thomas-cole?x=1",
            "https://www.wikiart.org/EN/thomas-cole",
            "https://www.wikiart.org/en/thomas.cole",
            "https://www.wikiart.org/en/",
            "https://www.wikiart.org.evil.com/en/thomas-cole",
            "https://evilwikiart.org/en/thomas-cole",
            "https://uploads.wikiart.org/en/thomas-cole",
            "https://www.wikiart.org:444/en/thomas-cole",
            "https://u:p@www.wikiart.org/en/thomas-cole",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(search_title("the-departure-1838"), "the-departure");
        assert_eq!(search_title("the-departure"), "the-departure");
    }

    fn painting(id: &str) -> Value {
        json!({"id": id, "title": format!("Painting {id}"), "artistName": "Thomas Cole",
            "year": "1838",
            "image": format!("https://uploads0.wikiart.org/images/thomas-cole/{id}.jpg")})
    }

    #[tokio::test]
    async fn artist_paginates_until_empty() {
        let (root, server) = serve(vec![
            json(json!({"artistName": "Thomas Cole", "url": "thomas-cole"})),
            json(json!({"Paintings": [painting("a1"), painting("a2")]})),
            json(json!({"Paintings": [painting("a3")]})),
            json(json!({"Paintings": null})),
        ]);
        let http = http("wikiart", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.wikiart.org/en/thomas-cole").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["artist"]["artistName"], "Thomas Cole");
        assert_eq!(k["filename"], "a1");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "a1_Painting a1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("wikiart/Thomas Cole")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(identity, r#"["{id}",["a3"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /en/thomas-cole?json=2 "));
        assert!(
            requests[1].starts_with(
                "GET /en/thomas-cole/mode/all-paintings?json=2&layout=new&page=1&resultType=masonry "
            ),
            "{}",
            requests[1]
        );
        assert!(requests[1].contains("x-requested-with: XMLHttpRequest"));
        assert!(
            requests[1]
                .contains("referer: https://www.wikiart.org/en/thomas-cole/mode/all-paintings")
        );
        assert!(requests[3].contains("&page=3&"));
    }

    #[tokio::test]
    async fn image_searches_one_page() {
        let (root, server) = serve(vec![
            json(json!({"artistName": "Thomas Cole"})),
            json(json!({"Paintings": [painting("d1")]})),
        ]);
        let http = http("wikiart", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.wikiart.org/en/thomas-cole/the-departure-1838").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "image");
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("GET /en/Search/Thomas%20Cole%20the-departure?json=2&"),
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn artworks_directory_capitalizes_group() {
        let (root, _server) = serve(vec![
            json(json!({"Paintings": [painting("g1")]})),
            json(json!({"Paintings": []})),
        ]);
        let http = http("wikiart", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.wikiart.org/en/paintings-by-media/grisaille").unwrap(),
        ))
        .await;
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("wikiart/Artworks by Media/grisaille")
        );
    }

    #[tokio::test]
    async fn artists_queue_artist_pages() {
        let (root, server) = serve(vec![
            json(
                json!({"Artists": [{"artistUrl": "/en/thomas-cole", "title": "Thomas Cole"},
                                    {"artistUrl": "https://evil.test/x"}]}),
            ),
            json(json!({"artistName": "Thomas Cole"})),
            json(json!({"Paintings": [painting("q1")]})),
            json(json!({"Paintings": []})),
            json(json!({"Artists": []})),
        ]);
        let http = http("wikiart", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.wikiart.org/en/artists-by-art-movement/academicism").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "artist");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /en/App/Search/Artists-by-art-movement?json=3&layout=new&page=1\
                 &resultType=masonry&searchterm=academicism "
            ),
            "{}",
            requests[0]
        );
        assert!(requests[4].contains("&page=2&"));
    }

    #[tokio::test]
    async fn invalid_artist_response_is_an_error() {
        let (root, server) = serve(vec![json(json!([]))]);
        let http = http("wikiart", root);
        let events = extract(
            &http,
            Url::parse("https://www.wikiart.org/en/thomas-cole").unwrap(),
        );
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
