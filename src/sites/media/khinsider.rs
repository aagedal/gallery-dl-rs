//! KHInsider game soundtracks, mirroring gallery_dl/extractor/khinsider.py:
//! the album page holds the metadata and track list, each track page links
//! its file formats.
//!
//! Options: `format` (default `mp3`; a comma-separated list, a list, or
//! `all`) and `covers`.
use super::path_segment;
use crate::error;
use crate::sites::art::{Scan, extr, extract_iter, nameext_into, parse_int, split_html, update};
use crate::sites::{Events, Http, Item, Route, Site, remove_html};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://downloads.khinsider.com";
static SOUNDTRACK: Route = Route::new(
    "soundtrack",
    "{filename}.{extension}",
    &["{category}", "{album[name]}"],
    "{filename}.{extension}",
);

pub static SITE: Site = Site {
    category: "khinsider",
    name: "KHInsider",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SOUNDTRACK],
    api_key: false,
    cookies: None,
};

/// `downloads\.khinsider\.com/game-soundtracks/album/([^/?#]+)`.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some("downloads.khinsider.com") {
        return None;
    }
    let album = url.path().strip_prefix("/game-soundtracks/album/")?;
    let album = album.split('/').next()?;
    path_segment(album).then(|| album.into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(album) = target(&url) else {
        return crate::sites::fail(error("unsupported KHInsider URL"));
    };
    crate::sites::once(async move {
        let url = http.api(&format!("{ROOT}/game-soundtracks/album/{album}"))?;
        let page = http.text(&url).await?;
        if !page.contains("Download all songs at once:") {
            return Err(error("KHInsider soundtrack not found"));
        }
        let data = metadata(&page);
        let mut events = Vec::new();
        if http.option_bool("covers", false) {
            let covers = covers(&page)
                .into_iter()
                .enumerate()
                .filter_map(|(index, raw)| {
                    let mut cover = Map::new();
                    cover.insert("url".into(), raw.as_str().into());
                    cover.insert("num".into(), (index + 1).into());
                    cover.insert("type".into(), "cover".into());
                    nameext_into(&raw, &mut cover);
                    update(&mut cover, &data);
                    Some(item(super::web_url(&raw)?, cover))
                })
                .collect();
            events = crate::sites::post(&SITE, http, &SOUNDTRACK, covers);
        }
        let tracks = track_pages(&page);
        let formats = formats(http);
        let tracks = futures_util::stream::iter(tracks.into_iter().enumerate()).then(
            move |(index, track)| {
                let data = data.clone();
                let formats = formats.clone();
                async move {
                    let page = http.text(&http.api(track.as_str())?).await?;
                    let items = track_files(&page, index + 1, formats.as_deref(), &data);
                    Ok(crate::sites::events(crate::sites::post(
                        &SITE,
                        http,
                        &SOUNDTRACK,
                        items,
                    )))
                }
            },
        );
        Ok(crate::sites::events(events)
            .chain(crate::sites::pages(tracks))
            .boxed())
    })
}

/// `metadata(page)`: the `album` keyword.
fn metadata(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut album = Map::new();
    album.insert(
        "name".into(),
        crate::sites::unescape(scan.extr("<h2>", "<")).into(),
    );
    let platforms: Vec<Value> = split_html(scan.extr("Platforms: ", "<br>"))
        .into_iter()
        .step_by(2)
        .map(Value::from)
        .collect();
    album.insert("platform".into(), platforms.into());
    album.insert("year".into(), scan.extr("Year: <b>", "<").into());
    album.insert(
        "catalog".into(),
        scan.extr("Catalog Number: <b>", "<").into(),
    );
    album.insert(
        "developer".into(),
        remove_html(scan.extr(" Developed by: ", "</")).into(),
    );
    album.insert(
        "publisher".into(),
        remove_html(scan.extr(" Published by: ", "</")).into(),
    );
    album.insert(
        "count".into(),
        parse_int(scan.extr("Number of Files: <b>", "<"), 0).into(),
    );
    let size = scan.extr("Total Filesize: <b>", "<");
    let size = size.char_indices().last().map_or("", |(i, _)| &size[..i]);
    album.insert(
        "size".into(),
        crate::sites::boorus::parse_bytes(size).into(),
    );
    album.insert("date".into(), scan.extr("Date Added: <b>", "<").into());
    album.insert(
        "type".into(),
        remove_html(scan.extr("Album type: <b>", "</b>")).into(),
    );
    album.insert(
        "uploader".into(),
        remove_html(scan.extr("Uploaded by: ", "</")).into(),
    );
    album.insert(
        "description".into(),
        scan.extr("<h2>Description</h2>", "<h2>").trim().into(),
    );
    let mut data = Map::new();
    data.insert("album".into(), album.into());
    data
}

/// The configured track formats; `None` accepts every format.
fn formats(http: &Http) -> Option<Vec<String>> {
    match http.option("format") {
        None => Some(vec!["mp3".into()]),
        Some(Value::String(format)) if format == "all" => None,
        Some(Value::String(format)) if !format.is_empty() => Some(
            format
                .to_lowercase()
                .split(',')
                .map(str::to_owned)
                .collect(),
        ),
        Some(Value::Array(formats)) if !formats.is_empty() => {
            Some(formats.iter().map(crate::sites::scalar).collect())
        }
        Some(_) => None,
    }
}

/// `_extract_covers`.
fn covers(page: &str) -> Vec<String> {
    extract_iter(page, " class=\"albumImage", "</")
        .into_iter()
        .map(|cover| crate::sites::unescape(extr(cover, " href=\"", "\"")))
        .collect()
}

/// The track page links of the song list, kept on KHInsider.
fn track_pages(page: &str) -> Vec<Url> {
    let list = extr(page, "<table id=\"songlist\">", "</table>");
    extract_iter(list, "<td class=\"clickable-row\"><a href=\"", "\"")
        .into_iter()
        .filter_map(|raw| {
            let url = Url::parse(ROOT).ok()?.join(raw).ok()?;
            (crate::sites::plain(&url) && url.host_str() == Some("downloads.khinsider.com"))
                .then_some(url)
        })
        .collect()
}

/// `_extract_tracks` for one track page: each file of a wanted format, or
/// the first file when none is.
fn track_files(
    page: &str,
    num: usize,
    formats: Option<&[String]>,
    data: &Map<String, Value>,
) -> Vec<Item> {
    let mut wanted = Vec::new();
    let mut first = None;
    for raw in extract_iter(page, "<p><a href=\"", "\"") {
        let mut track = Map::new();
        track.insert("num".into(), num.into());
        track.insert("url".into(), raw.into());
        nameext_into(raw, &mut track);
        let extension = crate::sites::scalar(&track["extension"]);
        if first.is_none() {
            first = Some(track.clone());
        }
        if formats.is_none_or(|formats| formats.contains(&extension)) {
            wanted.push(track);
        }
    }
    if wanted.is_empty() {
        wanted.extend(first);
    }
    wanted
        .into_iter()
        .filter_map(|mut track| {
            let url = super::web_url(&crate::sites::scalar(&track["url"]))?;
            update(&mut track, data);
            track.insert("type".into(), "track".into());
            Some(item(url, track))
        })
        .collect()
}

fn item(url: Url, data: Map<String, Value>) -> Item {
    let album = data
        .get("album")
        .and_then(|album| album.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let num = data
        .get("num")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Item::new(url, data)
        .ids(album.clone().unwrap_or_default(), num)
        .caption(album)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://downloads.khinsider.com/game-soundtracks/album/horizon-riders-wii"),
            Some("horizon-riders-wii".into())
        );
        assert_eq!(
            t("http://downloads.khinsider.com/game-soundtracks/album/x/01.mp3?y"),
            Some("x".into())
        );
        for raw in [
            "https://downloads.khinsider.com/game-soundtracks/album/",
            "https://downloads.khinsider.com/game-soundtracks/browse/a",
            "https://khinsider.com/game-soundtracks/album/x",
            "https://www.downloads.khinsider.com/game-soundtracks/album/x",
            "https://downloads.khinsider.com.evil.test/game-soundtracks/album/x",
            "https://downloads.khinsider.com:8080/game-soundtracks/album/x",
            "https://u:p@downloads.khinsider.com/game-soundtracks/album/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const ALBUM: &str = "<h2>Horizon &amp; Riders</h2>\
        Platforms: <a>Wii</a>, <a>PC</a><br>Year: <b>2008</b>\
        <br> Developed by: <a>Dev Co</a></br> Published by: <a>Pub</a></br>\
        Number of Files: <b>2</b> Total Filesize: <b>1.5 MB</b>\
        Date Added: <b>Jan 1st, 2020</b> Album type: <b><a>Soundtrack</a></b>\
        Uploaded by: <a>Up</a></a> <h2>Description</h2> Nice music <h2>\
        Download all songs at once: <a class=\"albumImage\" href=\"https://vgmsite.com/x/Cover%201.jpg\"></a>\
        <table id=\"songlist\"><td class=\"clickable-row\"><a href=\"/game-soundtracks/album/hr/01.mp3\">1</a>\
        <td class=\"clickable-row\"><a href=\"/game-soundtracks/album/hr/02.mp3\">2</a></table>";

    #[tokio::test]
    async fn soundtrack_tracks_by_format() {
        let (root, server) = serve(vec![
            html(ALBUM),
            html(
                "<p><a href=\"https://vgmsite.com/hr/01%20Intro.mp3\"><p><a href=\"https://vgmsite.com/hr/01%20Intro.flac\">",
            ),
            html("<p><a href=\"https://vgmsite.com/hr/02%20Outro.ogg\">"),
        ]);
        let http = http("khinsider", root);
        let files = media(extract(
            &http,
            Url::parse("https://downloads.khinsider.com/game-soundtracks/album/hr").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "01 Intro");
        assert_eq!(k["extension"], "mp3");
        assert_eq!(k["num"], 1);
        assert_eq!(k["type"], "track");
        assert_eq!(
            k["album"],
            json!({"name": "Horizon & Riders", "platform": ["Wii", "PC"], "year": "2008",
                "catalog": "", "developer": "Dev Co", "publisher": "Pub", "count": 2,
                "size": 1_572_864, "date": "Jan 1st, 2020", "type": "Soundtrack",
                "uploader": "Up", "description": "Nice music"})
        );
        assert_eq!(keywords(&files[1])["extension"], "ogg");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01 Intro.mp3");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("khinsider/Horizon & Riders")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{filename}.{extension}",["02 Outro","ogg"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /game-soundtracks/album/hr "));
        assert!(requests[1].starts_with("GET /game-soundtracks/album/hr/01.mp3 "));
        assert!(requests[2].starts_with("GET /game-soundtracks/album/hr/02.mp3 "));
    }

    #[tokio::test]
    async fn covers_and_all_formats() {
        let (root, server) = serve(vec![
            html(ALBUM),
            html(
                "<p><a href=\"https://vgmsite.com/hr/a.mp3\"><p><a href=\"https://vgmsite.com/hr/a.flac\">",
            ),
            html("<p>none</p>"),
        ]);
        let options = json!({"covers": true, "format": "all"});
        let http = http("khinsider", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://downloads.khinsider.com/game-soundtracks/album/hr").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["type"], "cover");
        assert_eq!(k["filename"], "Cover 1");
        assert_eq!(keywords(&files[2])["extension"], "flac");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn missing_album_is_an_error() {
        let (root, server) = serve(vec![html("<h2>Not found</h2>")]);
        let http = http("khinsider", root);
        let events = extract(
            &http,
            Url::parse("https://downloads.khinsider.com/game-soundtracks/album/x").unwrap(),
        );
        assert!(crate::sites::testing::fails(events).await);
        server.join().unwrap();
    }
}
