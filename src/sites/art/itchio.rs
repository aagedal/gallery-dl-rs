//! itch.io game downloads, mirroring gallery_dl/extractor/itchio.py: the game
//! page lists upload ids, and each upload's lightbox request (authorized by
//! the `itchio_token` CSRF cookie the page sets) names its file URL.
//!
//! Only free uploads are offered this way; paid ones need a purchase.
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static GAME: Route = Route::new(
    "game",
    "{game[title]} ({id}).{extension}",
    &["{category}", "{user[name]}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "itchio",
    name: "itch.io",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GAME],
    api_key: false,
    cookies: Some("itch.io"),
};

/// Python `\w` restricted to ASCII.
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `(\w+)\.itch\.io/([\w-]+)`: one subdomain label and the leading
/// `[\w-]` run of the first path segment.
fn target(url: &Url) -> Option<(String, String)> {
    if !crate::sites::plain(url) {
        return None;
    }
    let user = url.host_str()?.strip_suffix(".itch.io")?;
    if user.is_empty() || !user.bytes().all(word) {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let len = path.bytes().take_while(|b| word(*b) || *b == b'-').count();
    (len > 0).then(|| (user.to_owned(), path[..len].to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((user, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported itch.io URL"));
    };
    crate::sites::once(async move {
        let game_url = format!("https://{user}.itch.io/{slug}");
        let page_url = http.api(&game_url)?;
        let page = http.text(&page_url).await?;
        let token = http
            .cookie(&page_url, "itchio_token")
            .ok_or_else(|| error("itch.io set no CSRF token"))?;
        let token = crate::sites::percent_decode(&token);
        let origin = format!("https://{user}.itch.io");
        let headers = [
            ("Referer", game_url.as_str()),
            ("X-Requested-With", "XMLHttpRequest"),
            ("Origin", origin.as_str()),
        ];
        let mut items = Vec::new();
        for upload in super::extract_iter(&page, "data-upload_id=\"", "\"") {
            if !super::digits(upload) {
                continue;
            }
            let mut file_url = http.api(&format!("{game_url}/file/{upload}"))?;
            file_url
                .query_pairs_mut()
                .append_pair("source", "view_game")
                .append_pair("as_props", "1")
                .append_pair("after_download_lightbox", "true");
            let info = http
                .post_form(&file_url, &[("csrf_token", token.as_str())], &headers)
                .await?;
            items.extend(upload_item(&info, &game_url, upload));
        }
        // Each upload is its own Directory message upstream.
        let events = items
            .into_iter()
            .flat_map(|item| crate::sites::post(&SITE, http, &GAME, vec![item]))
            .collect();
        Ok(crate::sites::events(events))
    })
}

/// One upload's lightbox response as upstream keywords.
pub(crate) fn upload_item(info: &Value, game_url: &str, upload: &str) -> Option<Item> {
    let raw = super::str_of(info, "url");
    let url = super::file_url(raw, game_url)?;
    let lightbox = info.get("lightbox")?;
    let mut game = lightbox.get("game")?.as_object()?.clone();
    let mut user = lightbox.get("user")?.as_object()?.clone();
    game.insert("url".into(), game_url.into());
    user.remove("follow_button");
    let title = game.get("title").and_then(Value::as_str).map(str::to_owned);
    let name = user.get("name").and_then(Value::as_str).map(str::to_owned);
    let mut data = Map::new();
    data.insert("game".into(), game.into());
    data.insert("user".into(), user.into());
    data.insert("id".into(), upload.into());
    super::nameext_into(raw, &mut data);
    Some(
        Item::new(url, data)
            .ids(upload, upload)
            .user(name)
            .caption(title),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://sirtartarus.itch.io/a-craft-of-mine"),
            Some(("sirtartarus".into(), "a-craft-of-mine".into()))
        );
        assert_eq!(
            t("http://user_1.itch.io/game.html?x#y"),
            Some(("user_1".into(), "game".into()))
        );
        for raw in [
            "https://itch.io/game",
            "https://user.itch.io/",
            "https://user.itch.io/.x",
            "https://a.b.itch.io/game",
            "https://user-name.itch.io/game",
            "https://user.itch.io.evil.test/game",
            "https://user.evilitch.io/game",
            "https://user.itch.io:8443/game",
            "https://u:p@user.itch.io/game",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn lightbox(id: u64, file: &str) -> Value {
        json!({"url": format!("https://w3g3a5v6.ssl.hwcdn.net/upload2/game/1/{file}?token=x"),
        "lightbox": {
            "game": {"id": 1, "title": "Game Title", "cover_image": "c.png"},
            "user": {"id": 2, "name": "Author", "url": "https://a.itch.io",
                "follow_button": "<div></div>"},
            "upload_id": id,
        }})
    }

    #[tokio::test]
    async fn extracts_uploads() {
        let page = response(
            "200 OK\r\nSet-Cookie: itchio_token=a%2Bb%3D; Path=/",
            "text/html",
            br#"<div class="upload" data-upload_id="111"></div>
<div class="upload" data-upload_id="222"></div>"#,
        );
        let (root, server) = serve(vec![
            page,
            json(lightbox(111, "first.zip")),
            json(lightbox(222, "Second%20File.TAR.GZ")),
        ]);
        let http = http("itchio", root);
        let files = media(extract(
            &http,
            Url::parse("https://author.itch.io/game-title").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://w3g3a5v6.ssl.hwcdn.net/upload2/game/1/first.zip?token=x"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["id"], "222");
        assert_eq!(k["game"]["url"], "https://author.itch.io/game-title");
        assert_eq!(k["game"]["title"], "Game Title");
        assert_eq!(
            k["user"],
            json!({"id": 2, "name": "Author", "url": "https://a.itch.io"})
        );
        assert_eq!(k["filename"], "Second File.TAR");
        assert_eq!(k["extension"], "gz");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "Game Title (222).gz");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("itchio/Author")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["111"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /game-title "));
        let post = requests[1].to_ascii_lowercase();
        assert!(post.starts_with(
            "post /game-title/file/111?source=view_game&as_props=1&after_download_lightbox=true "
        ));
        assert!(post.contains("x-requested-with: xmlhttprequest"));
        assert!(post.contains("origin: https://author.itch.io"));
        assert!(post.contains("referer: https://author.itch.io/game-title"));
        assert!(requests[1].ends_with("csrf_token=a%2Bb%3D"));
    }

    #[tokio::test]
    async fn missing_token_is_an_error() {
        let (root, server) = serve(vec![html(r#"<div data-upload_id="1"></div>"#)]);
        let http = http("itchio", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://a.itch.io/g").unwrap()
            ))
            .await
        );
        server.join().unwrap();
    }
}
