//! Speaker Deck presentations, mirroring gallery_dl/extractor/speakerdeck.py:
//! the oEmbed endpoint names the player, whose page lists the slide images.
use super::{extr, extract_iter, on_host, raw_segments, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://speakerdeck.com";
static PRESENTATION: Route = Route::new(
    "presentation",
    "{presentation}-{num:>02}.{extension}",
    &["{category}", "{user}"],
    "{presentation}_{num}",
);

pub static SITE: Site = Site {
    category: "speakerdeck",
    name: "Speaker Deck",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&PRESENTATION],
    api_key: false,
};

/// `speakerdeck\.com/([^/?#]+)/([^/?#]+)`, raw as upstream sees them.
fn target(url: &Url) -> Option<(String, String)> {
    if !on_host(url, &["speakerdeck.com"]) {
        return None;
    }
    match raw_segments(url).as_slice() {
        [user, presentation, ..] if url.path().starts_with(&format!("/{user}/{presentation}")) => {
            Some(((*user).to_owned(), (*presentation).to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((user, presentation)) = target(&url) else {
        return crate::sites::fail(error("unsupported Speaker Deck URL"));
    };
    crate::sites::once(async move {
        let mut url = http.api(&format!("{ROOT}/oembed.json"))?;
        url.query_pairs_mut()
            .append_pair("url", &format!("{ROOT}/{user}/{presentation}"));
        let oembed = http.json(&url).await?;
        let data = metadata(&user, &presentation, &oembed);
        let id = str_of(&Value::Object(data.clone()), "presentation_id").to_owned();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(error("Speaker Deck presentation has no player"));
        }
        let page = http
            .text(&http.api(&format!("{ROOT}/player/{id}"))?)
            .await?;
        let items = parse_player(&page, &data)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &PRESENTATION,
            items,
        )))
    })
}

pub(crate) fn metadata(user: &str, presentation: &str, oembed: &Value) -> Map<String, Value> {
    let id = extr(
        str_of(oembed, "html"),
        "src=\"//speakerdeck.com/player/",
        "\"",
    );
    let mut data = Map::new();
    data.insert("user".into(), user.into());
    data.insert("presentation".into(), presentation.into());
    data.insert("presentation_id".into(), id.into());
    data.insert(
        "title".into(),
        oembed.get("title").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "author".into(),
        oembed.get("author_name").cloned().unwrap_or(Value::Null),
    );
    data
}

/// GalleryExtractor numbering over the player's slide images.
pub(crate) fn parse_player(page: &str, data: &Map<String, Value>) -> Result<Vec<Item>> {
    let page = page.split_whitespace().collect::<Vec<_>>().join(" ");
    let urls: Vec<Url> = extract_iter(&page, "js-sd-slide\" data-url=\"", "\"")
        .into_iter()
        .filter_map(|raw| super::file_url(raw, ROOT))
        .collect();
    let count = urls.len();
    let id = data.get("presentation_id").map(crate::sites::scalar);
    let user = data.get("user").and_then(Value::as_str).map(str::to_owned);
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    Ok(urls
        .into_iter()
        .enumerate()
        .map(|(index, url)| {
            let mut fields = data.clone();
            fields.insert("count".into(), count.into());
            fields.insert("num".into(), (index + 1).into());
            super::nameext_into(url.as_str(), &mut fields);
            Item::new(url, fields)
                .ids(id.clone().unwrap_or_default(), (index + 1).to_string())
                .user(user.clone())
                .caption(title.clone())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://speakerdeck.com/speakerdeck/introduction-to-speakerdeck"),
            Some(("speakerdeck".into(), "introduction-to-speakerdeck".into()))
        );
        assert!(t("https://www.speakerdeck.com/u/p/extra").is_some());
        for raw in [
            "https://speakerdeck.com/user",
            "https://speakerdeck.com//p",
            "https://speakerdeck.com.evil.com/u/p",
            "https://evilspeakerdeck.com/u/p",
            "https://a.speakerdeck.com/u/p",
            "https://speakerdeck.com:8080/u/p",
            "https://u:p@speakerdeck.com/u/p",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_slides() {
        let oembed = json!({"title": "Intro", "author_name": "Speaker Deck",
            "html": "<iframe src=\"//speakerdeck.com/player/abc123\" ></iframe>"});
        let player = "<div class=\"js-sd-slide\"\n data-url=\"https://files.speakerd.com/p/abc123/slide_0.jpg?1\"></div>\
            <div class=\"js-sd-slide\" data-url=\"https://files.speakerd.com/p/abc123/slide_1.jpg?2\"></div>";
        let (root, server) = serve(vec![json(oembed), html(player)]);
        let http = http("speakerdeck", root);
        let files = media(extract(
            &http,
            Url::parse("https://speakerdeck.com/sd/intro").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["presentation_id"], "abc123");
        assert_eq!(k["author"], "Speaker Deck");
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "slide_1");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "intro-02.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("speakerdeck/sd")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{presentation}_{num}",["intro","1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /oembed.json?url=https%3A%2F%2Fspeakerdeck.com%2Fsd%2Fintro "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /player/abc123 "));
    }

    #[tokio::test]
    async fn oembed_without_player_is_an_error() {
        let (root, server) = serve(vec![json(
            json!({"title": "T", "author_name": "A", "html": ""}),
        )]);
        let http = http("speakerdeck", root);
        let events = extract(&http, Url::parse("https://speakerdeck.com/u/p").unwrap());
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
