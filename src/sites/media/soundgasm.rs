//! soundgasm audio and users, mirroring gallery_dl/extractor/soundgasm.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, nameext_into, re, remove_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://soundgasm.net";

static AUDIO: Route = Route::new(
    "audio",
    "{title}.{extension}",
    &["{category}", "{user}"],
    "{user}_{slug}",
);
static USER: Route = Route::new(
    "user",
    "{title}.{extension}",
    &["{category}", "{user}"],
    "{user}_{slug}",
);

pub static SITE: Site = Site {
    category: "soundgasm",
    name: "soundgasm",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&AUDIO, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Audio(String, String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?soundgasm\.net/u(?:ser)?";
    if let Some(c) = captures(re!(&format!(r"{BASE}/([^/?#]+)/([^/?#]+)")), url) {
        return Some(Target::Audio(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(&format!(r"{BASE}/([^/?#]+)/?$")), url).map(|c| Target::User(group(&c, 1).into()))
}

/// `_extract_sound`.
async fn sound(http: &Http, url: &str) -> crate::Result<Item> {
    let page = http.text(&http.api(url)?).await?;
    let mut scan = Scan::new(&page);
    let mut parts = url.trim_end_matches('/').rsplitn(3, '/');
    let slug = parts.next().unwrap_or_default().to_owned();
    let user = parts.next().unwrap_or_default().to_owned();
    let mut data = Map::new();
    data.insert("user".into(), user.into());
    data.insert("slug".into(), slug.as_str().into());
    data.insert(
        "title".into(),
        unescape(scan.extr("aria-label=\"title\">", "<")).into(),
    );
    data.insert(
        "description".into(),
        unescape(&remove_html(
            scan.extr("class=\"jp-description\">", "</div>"),
        ))
        .into(),
    );
    let file = extr(scan.extr("\"setMedia\", {", "}"), ": \"", "\"").to_owned();
    data.insert("url".into(), file.as_str().into());
    nameext_into(&file, &mut data);
    let file = Url::parse(&file).map_err(|_| error("soundgasm sound has no audio URL"))?;
    Ok(Item::new(file, data).ids(slug.clone(), slug))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported soundgasm URL"));
    };
    match target {
        Target::Audio(user, slug) => crate::sites::once(async move {
            let item = sound(http, &format!("{ROOT}/u/{user}/{slug}")).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &AUDIO,
                vec![item],
            )))
        }),
        Target::User(user) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/user/{user}"))?)
                .await?;
            let sounds: Vec<String> = extract_iter(&page, "class=\"sound-details\">", "</a>")
                .into_iter()
                .map(|s| extr(s, "<a href=\"", "\"").to_owned())
                .collect();
            Ok(futures_util::stream::iter(sounds)
                .then(move |url| async move {
                    match sound(http, &url).await {
                        Ok(item) => {
                            crate::sites::events(crate::sites::post(&SITE, http, &USER, vec![item]))
                        }
                        Err(e) => crate::sites::fail(e),
                    }
                })
                .flatten()
                .boxed())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    const SOUND: &str = r##"<div aria-label="title">Otto &amp; von</div><div class="jp-description"><p>Desc</p></div>
        $("#jquery_jplayer_1").jPlayer("setMedia", { m4a: "https://media.soundgasm.net/sounds/abc.m4a" });"##;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.soundgasm.net/user/Class/687-Otto"),
            Some(Target::Audio("Class".into(), "687-Otto".into()))
        );
        assert_eq!(
            t("https://soundgasm.net/u/fierce-aphrodite"),
            Some(Target::User("fierce-aphrodite".into()))
        );
    }

    #[tokio::test]
    async fn extracts_user_sounds() {
        let list = r#"<div class="sound-details"><a href="https://soundgasm.net/u/Class/687-Otto">x</a></div>"#;
        let (root, server) = serve(vec![html(list), html(SOUND)]);
        let http = client("soundgasm", root);
        let files = media(extract(
            &http,
            Url::parse("https://soundgasm.net/u/Class").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://media.soundgasm.net/sounds/abc.m4a"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Otto & von");
        assert_eq!(k["user"], "Class");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Otto & von.m4a");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("soundgasm/Class")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /user/Class "));
        assert!(requests[1].starts_with("GET /u/Class/687-Otto "));
    }
}
