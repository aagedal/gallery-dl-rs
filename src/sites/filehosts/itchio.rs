//! itch.io game downloads, mirroring gallery_dl/extractor/itchio.py.
use crate::error;
use crate::sites::gdl::{captures, extract_iter, group, nameext_into, re, unquote};
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

fn target(url: &Url) -> Option<(String, String)> {
    captures(re!(r"^https?://(\w+)\.itch\.io/([\w-]+)"), url)
        .map(|c| (group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((user, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported itch.io URL"));
    };
    crate::sites::once(async move {
        let game_url = format!("https://{user}.itch.io/{slug}");
        let request = http.api(&game_url)?;
        let page = http.text(&request).await?;
        let token = http
            .cookie(&request, "itchio_token")
            .ok_or_else(|| error("itch.io did not set an itchio_token cookie"))?;
        let token = unquote(&token);
        let origin = format!("https://{user}.itch.io");
        let headers = [
            ("Referer", game_url.as_str()),
            ("X-Requested-With", "XMLHttpRequest"),
            ("Origin", origin.as_str()),
        ];
        let mut items = Vec::new();
        for upload_id in extract_iter(&page, "data-upload_id=\"", "\"") {
            let mut file_url = http.api(&format!("{game_url}/file/{upload_id}"))?;
            file_url
                .query_pairs_mut()
                .append_pair("source", "view_game")
                .append_pair("as_props", "1")
                .append_pair("after_download_lightbox", "true");
            let info = http
                .post_form(&file_url, &[("csrf_token", token.as_str())], &headers)
                .await?;
            let mut game = info["lightbox"]["game"].clone();
            let mut user = info["lightbox"]["user"].clone();
            if let Some(game) = game.as_object_mut() {
                game.insert("url".into(), game_url.as_str().into());
            }
            if let Some(user) = user.as_object_mut() {
                user.remove("follow_button");
            }
            let mut data = Map::new();
            data.insert("game".into(), game);
            data.insert("user".into(), user);
            data.insert("id".into(), upload_id.into());
            let url = info.get("url").and_then(Value::as_str).unwrap_or_default();
            nameext_into(url, &mut data);
            if let Ok(file) = Url::parse(url) {
                items.push(Item::new(file, data).ids(upload_id, upload_id));
            }
        }
        Ok(crate::sites::events(
            items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &GAME, vec![item]))
                .collect(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, media, response, serve};
    use serde_json::json;

    #[tokio::test]
    async fn posts_for_each_upload() {
        let page = response(
            "200 OK\r\nSet-Cookie: itchio_token=a%2Fb; Path=/",
            "text/html",
            br#"<a data-upload_id="7"></a>"#,
        );
        let info = json!({"url": "https://dl.itch.zone/abc/game.zip?x=1", "lightbox": {"game": {"title": "Game"}, "user": {"name": "dev", "follow_button": "x"}}});
        let (root, server) = serve(vec![page, json(info)]);
        let http = client("itchio", root);
        let files = media(extract(
            &http,
            Url::parse("https://dev.itch.io/game").unwrap(),
        ))
        .await;
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Game (7).zip");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("itchio/dev")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "POST /game/file/7?source=view_game&as_props=1&after_download_lightbox=true "
        ));
        assert!(requests[1].ends_with("csrf_token=a%2Fb"));
    }
}
