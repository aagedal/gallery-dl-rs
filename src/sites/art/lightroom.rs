//! Adobe Lightroom shared albums, mirroring
//! gallery_dl/extractor/lightroom.py: the share page embeds the album
//! attributes, and the album's resource listing is paginated JSON.
use super::{extr, parse_int, raw_segments, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    "{num:>04}_{id}.{extension}",
    &["{category}", "{user}", "{title}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "lightroom",
    name: "Lightroom",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

/// `lightroom\.adobe\.com/shares/([0-9a-f]+)`, unanchored after the id.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some("lightroom.adobe.com") {
        return None;
    }
    let [shares, id, ..] = raw_segments(url)[..] else {
        return None;
    };
    let len = id
        .bytes()
        .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        .count();
    (shares == "shares" && (1..=128).contains(&len)).then(|| id[..len].to_owned())
}

/// Album `gallery_id`, `user` and `title` keywords.
fn metadata(href: &str, album: &Value) -> Map<String, Value> {
    let payload = album.get("payload").unwrap_or(&Value::Null);
    let story = payload
        .get("story")
        .filter(|story| super::truthy(Some(story)))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let mut data = Map::new();
    data.insert("gallery_id".into(), href.into());
    data.insert(
        "user".into(),
        story.get("author").cloned().unwrap_or_else(|| "".into()),
    );
    data.insert(
        "title".into(),
        story
            .get("title")
            .or_else(|| payload.get("name"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    data
}

struct State {
    base: String,
    next: String,
    num: u64,
    album: Map<String, Value>,
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(href) = target(&url) else {
        return crate::sites::fail(error("unsupported Lightroom URL"));
    };
    crate::sites::once(async move {
        let url = http.api(&format!("https://lightroom.adobe.com/shares/{href}"))?;
        let page = http.text(&url).await?;
        let album = super::json_loads(extr(&page, "albumAttributes: ", "\n"), "Lightroom")?;
        let next = album
            .pointer("/links/~1rels~1space_album_images_videos/href")
            .and_then(Value::as_str)
            .ok_or_else(|| error("Lightroom album has no image listing"))?
            .to_owned();
        let state = State {
            base: str_of(&album, "base").to_owned(),
            next,
            num: 1,
            album: metadata(&href, &album),
        };
        Ok(pages(http, state))
    })
}

fn pages(http: &Http, state: State) -> Events<'_> {
    crate::sites::paginate(Some(state), move |state| async move {
        let Some(mut state) = state else {
            return Ok(None);
        };
        let url = http.api(&format!("{}{}", state.base, state.next))?;
        let page = http.text(&url).await?;
        // The first line is a JavaScript loop guard.
        let body = page
            .split_once('\n')
            .map(|(_, body)| body)
            .ok_or_else(|| error("Lightroom listing has no JSON body"))?;
        let data = super::json_loads(body, "Lightroom")?;
        let (items, num) = parse_resources(&data, &state.album, state.num)?;
        let mut events = Vec::new();
        for item in items {
            events.extend(crate::sites::post(&SITE, http, &GALLERY, vec![item]));
        }
        let next = data
            .pointer("/links/next/href")
            .and_then(Value::as_str)
            .filter(|next| !next.is_empty() && *next != state.next)
            .map(str::to_owned);
        let next = next.map(|next| {
            state.base = str_of(&data, "base").to_owned();
            state.next = next;
            state.num = num;
            state
        });
        Ok(Some((crate::sites::events(events), next)))
    })
}

/// The largest rendition of each resource, numbered from `num`.
pub(crate) fn parse_resources(
    data: &Value,
    album: &Map<String, Value>,
    mut num: u64,
) -> Result<(Vec<Item>, u64)> {
    let base = str_of(data, "base");
    let mut items = Vec::new();
    for res in data
        .get("resources")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Lightroom listing has no resources"))?
    {
        let Some(asset) = res.get("asset") else {
            continue;
        };
        let mut best: Option<&str> = None;
        let mut size = 0;
        for (key, value) in asset
            .get("links")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if !key.starts_with("/rels/rendition_type/") {
                continue;
            }
            let candidate = parse_int(key.rsplit('/').next().unwrap_or_default(), 0);
            if candidate > size {
                size = candidate;
                best = value.get("href").and_then(Value::as_str);
            }
        }
        let Some(href) = best else {
            continue;
        };
        let raw = format!("{base}{href}");
        let Some(url) = super::file_url(&raw, "https://lightroom.adobe.com/") else {
            continue;
        };
        let id = asset.get("id").cloned().unwrap_or(Value::Null);
        let mut fields = Map::new();
        fields.insert("id".into(), id.clone());
        fields.insert("num".into(), num.into());
        fields.insert("url".into(), raw.into());
        super::update(&mut fields, album);
        super::nameext_into(url.as_str(), &mut fields);
        let gallery = album.get("gallery_id").map(crate::sites::scalar);
        let user = album.get("user").and_then(Value::as_str).map(str::to_owned);
        let title = album
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(
            Item::new(url, fields)
                .ids(gallery.unwrap_or_default(), crate::sites::scalar(&id))
                .user(user)
                .caption(title),
        );
        num += 1;
    }
    Ok((items, num))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};
    use futures_util::StreamExt;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://lightroom.adobe.com/shares/0c9cce2033f24d24975423fe616368bf"),
            Some("0c9cce2033f24d24975423fe616368bf".into())
        );
        assert_eq!(
            t("https://lightroom.adobe.com/shares/abcxyz"),
            Some("abc".into())
        );
        for raw in [
            "https://lightroom.adobe.com/shares/",
            "https://lightroom.adobe.com/shares/xyz",
            "https://lightroom.adobe.com/libraries/abc",
            "https://www.lightroom.adobe.com/shares/abc",
            "https://lightroom.adobe.com.evil.com/shares/abc",
            "https://evillightroom.adobe.com/shares/abc",
            "https://lightroom.adobe.com:8443/shares/abc",
            "https://u:p@lightroom.adobe.com/shares/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn listing(name: &str, next: Option<&str>) -> String {
        let links = match next {
            Some(next) => format!(r#"{{"next": {{"href": "{next}"}}}}"#),
            None => "{}".into(),
        };
        format!(
            "while (1) {{}}\n{{\"base\": \"https://photos.adobe.io/v2/spaces/s/\", \"links\": {links}, \
             \"resources\": [{{\"asset\": {{\"id\": \"{name}\", \"links\": {{\
             \"/rels/rendition_type/640\": {{\"href\": \"assets/{name}/640\"}},\
             \"/rels/rendition_type/2048\": {{\"href\": \"assets/{name}/2048\"}},\
             \"self\": {{\"href\": \"x\"}}}}}}}}]}}"
        )
    }

    #[tokio::test]
    async fn paginates_album_resources() {
        let share = concat!(
            "<script>albumAttributes: {\"base\": \"https://photos.adobe.io/v2/spaces/s/\", ",
            "\"payload\": {\"name\": \"Album\", \"story\": {\"author\": \"Ann\"}}, ",
            "\"links\": {\"/rels/space_album_images_videos\": {\"href\": \"albums/a/assets\"}}}\n",
        );
        let (root, server) = serve(vec![
            html(share),
            html(&listing("one", Some("albums/a/assets?page=2"))),
            html(&listing("two", None)),
        ]);
        let http = http("lightroom", root);
        let files = media(extract(
            &http,
            Url::parse("https://lightroom.adobe.com/shares/abc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://photos.adobe.io/v2/spaces/s/assets/one/2048"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 2);
        assert_eq!(k["user"], "Ann");
        assert_eq!(k["title"], "Album");
        assert_eq!(k["gallery_id"], "abc");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        // The size component has no extension; upstream renders the dot too.
        assert_eq!(file.render(&files[1]).unwrap(), "0002_two.");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("lightroom/Ann/Album")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["one"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /shares/abc "));
        assert!(requests[1].starts_with("GET /v2/spaces/s/albums/a/assets "));
        assert!(requests[2].starts_with("GET /v2/spaces/s/albums/a/assets?page=2 "));
    }

    #[tokio::test]
    async fn share_without_album_is_an_error() {
        let (root, server) = serve(vec![html("<html></html>")]);
        let http = http("lightroom", root);
        let events = extract(
            &http,
            Url::parse("https://lightroom.adobe.com/shares/abc").unwrap(),
        );
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
