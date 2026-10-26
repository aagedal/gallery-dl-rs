//! KHInsider soundtracks, mirroring gallery_dl/extractor/khinsider.py.
//! Options: `format` (default `mp3`; `all` or a comma list) and `covers`.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, nameext_into, parse_bytes, parse_int, re,
    remove_html, split_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
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

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://downloads\.khinsider\.com/game-soundtracks/album/([^/?#]+)");
    captures(pattern, url).map(|c| group(&c, 1).to_owned())
}

fn metadata(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut album = Map::new();
    album.insert("name".into(), unescape(scan.extr("<h2>", "<")).into());
    let platforms: Vec<String> = split_html(scan.extr("Platforms: ", "<br>"))
        .into_iter()
        .step_by(2)
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
        parse_int(scan.extr("Number of Files: <b>", "<")).into(),
    );
    let size = scan.extr("Total Filesize: <b>", "<");
    album.insert(
        "size".into(),
        parse_bytes(size.get(..size.len().saturating_sub(1)).unwrap_or_default()).into(),
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
    data.insert("album".into(), Value::Object(album));
    data
}

/// The `format` option as a lowercase list; `None` means all formats.
fn formats(http: &Http) -> Option<Vec<String>> {
    match http.option("format") {
        None => Some(vec!["mp3".into()]),
        Some(Value::String(s)) if s == "all" => None,
        Some(Value::String(s)) if !s.is_empty() => {
            Some(s.to_lowercase().split(',').map(str::to_owned).collect())
        }
        Some(Value::Array(list)) if !list.is_empty() => {
            Some(list.iter().map(crate::sites::scalar).collect())
        }
        Some(_) => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(album) = target(&url) else {
        return crate::sites::fail(error("unsupported KHInsider URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text(&http.api(&format!("{ROOT}/game-soundtracks/album/{album}"))?)
            .await?;
        if !page.contains("Download all songs at once:") {
            return Err(error("KHInsider soundtrack not found"));
        }
        let data = metadata(&page);
        let mut items = Vec::new();
        if http.option_bool("covers", false) {
            for (index, cover) in extract_iter(&page, " class=\"albumImage", "</")
                .into_iter()
                .enumerate()
            {
                let raw = unescape(extr(cover, " href=\"", "\""));
                let Ok(url) = Url::parse(&raw) else { continue };
                let mut fields = data.clone();
                fields.insert("url".into(), raw.as_str().into());
                fields.insert("num".into(), (index + 1).into());
                fields.insert("type".into(), "cover".into());
                nameext_into(&raw, &mut fields);
                items.push(Item::new(url, fields));
            }
        }
        let wanted = formats(http);
        let songs = extr(&page, "<table id=\"songlist\">", "</table>");
        for (index, href) in extract_iter(songs, "<td class=\"clickable-row\"><a href=\"", "\"")
            .into_iter()
            .enumerate()
        {
            let song_url = Url::parse(ROOT)
                .and_then(|root| root.join(href))
                .map_err(|_| error("invalid KHInsider song URL"))?;
            let song = http.text(&http.api(song_url.as_str())?).await?;
            let mut first: Option<Item> = None;
            let mut matched = false;
            for raw in extract_iter(&song, "<p><a href=\"", "\"") {
                let Ok(url) = Url::parse(raw) else { continue };
                let mut fields = data.clone();
                fields.insert("num".into(), (index + 1).into());
                fields.insert("url".into(), raw.into());
                nameext_into(raw, &mut fields);
                fields.insert("type".into(), "track".into());
                let extension = fields
                    .get("extension")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                let item = Item::new(url, fields);
                if wanted.as_ref().is_none_or(|w| w.contains(&extension)) {
                    matched = true;
                    items.push(item);
                } else if first.is_none() && !matched {
                    first = Some(item);
                }
            }
            if !matched && let Some(first) = first {
                items.push(first);
            }
        }
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &SOUNDTRACK,
            items,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://downloads.khinsider.com/game-soundtracks/album/horizon-riders-wii")
                .as_deref(),
            Some("horizon-riders-wii")
        );
        assert_eq!(t("https://khinsider.com/game-soundtracks/album/x"), None);
    }

    #[tokio::test]
    async fn picks_requested_formats() {
        let album = r#"<h2>Horizon Riders</h2>Platforms: <a>Wii</a>, <a>PC</a><br>Year: <b>2009</b> Number of Files: <b>2</b>
            Total Filesize: <b>12.5 MB</b> Date Added: <b>Jan 1st, 2020</b> Download all songs at once:
            <table id="songlist"><tr><td class="clickable-row"><a href="/game-soundtracks/album/hr/01.mp3">1</a></td></tr>
            <tr><td class="clickable-row"><a href="/game-soundtracks/album/hr/02.mp3">2</a></td></tr></table>"#;
        let first = r#"<p><a href="https://vgmsite.com/hr/01.mp3">mp3</a><p><a href="https://vgmsite.com/hr/01.flac">flac</a>"#;
        let second = r#"<p><a href="https://vgmsite.com/hr/02.flac">flac</a>"#;
        let (root, server) = serve(vec![html(album), html(first), html(second)]);
        let http = client("khinsider", root);
        let files = media(extract(
            &http,
            Url::parse("https://downloads.khinsider.com/game-soundtracks/album/hr").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://vgmsite.com/hr/01.mp3");
        // Tracks without the requested format fall back to their first file.
        assert_eq!(files[1].url.as_str(), "https://vgmsite.com/hr/02.flac");
        let k = keywords(&files[0]);
        assert_eq!(k["album"]["platform"], serde_json::json!(["Wii", "PC"]));
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("khinsider/Horizon Riders")
        );
        server.join().unwrap();
    }
}
