//! soundgasm.net audio clips, mirroring gallery_dl/extractor/soundgasm.py:
//! each clip page names its title, description and `setMedia` source.
use crate::sites::{Events, Http, Item, Route, Site, extr, remove_html, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://soundgasm.net";
const FILENAME: &str = "{title}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user}"];
const ARCHIVE: &str = "{user}_{slug}";
static AUDIO: Route = Route::new("audio", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);

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
    Audio { user: String, slug: String },
    User(String),
}

/// `soundgasm\.net/u(?:ser)?/USER/SLUG` and `.../USER/?$`, raw as upstream
/// sees them.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !crate::sites::host_is(url, &["soundgasm.net"]) {
        return None;
    }
    let path = url.path();
    let rest = path
        .strip_prefix("/user/")
        .or_else(|| path.strip_prefix("/u/"))?;
    let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if user.is_empty() {
        return None;
    }
    let slug = tail.split('/').next().unwrap_or_default();
    if !slug.is_empty() {
        return Some(Target::Audio {
            user: user.into(),
            slug: slug.into(),
        });
    }
    // `/?$`: nothing may follow the user name but one slash.
    (tail.is_empty() && url.query().is_none() && url.fragment().is_none())
        .then(|| Target::User(user.into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Audio { user, slug }) => crate::sites::once(async move {
            sound_events(http, &AUDIO, &format!("{ROOT}/u/{user}/{slug}")).await
        }),
        Some(Target::User(user)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/user/{user}"))?)
                .await?;
            let sounds = sound_urls(&page);
            Ok(crate::sites::pages(futures_util::StreamExt::then(
                futures_util::stream::iter(sounds),
                move |sound| async move { sound_events(http, &USER, &sound).await },
            )))
        }),
        None => crate::sites::fail(error("unsupported soundgasm URL")),
    }
}

/// The clip links of a user page, kept only when they stay on soundgasm.
fn sound_urls(page: &str) -> Vec<String> {
    crate::sites::art::extract_iter(page, "class=\"sound-details\">", "</a>")
        .into_iter()
        .map(|sound| crate::sites::art::extr(sound, "<a href=\"", "\"").to_owned())
        .filter(|sound| {
            Url::parse(sound).is_ok_and(|url| matches!(target(&url), Some(Target::Audio { .. })))
        })
        .collect()
}

/// `_extract_sound` for one clip page; each clip is its own post.
async fn sound_events<'a>(http: &'a Http, route: &'static Route, url: &str) -> Result<Events<'a>> {
    let page = http.text(&http.api(url)?).await?;
    let data = parse_sound(url, &page);
    let raw = data.get("url").and_then(Value::as_str).unwrap_or_default();
    let Some(file) = super::web_url(raw) else {
        return Err(error("soundgasm page contains no audio"));
    };
    let mut keywords = data.clone();
    super::set_nameext(&mut keywords, crate::sites::nameext(&file));
    let id = format!(
        "{}_{}",
        keywords["user"].as_str().unwrap_or_default(),
        keywords["slug"].as_str().unwrap_or_default()
    );
    let user = keywords["user"].as_str().map(str::to_owned);
    let title = keywords["title"].as_str().map(str::to_owned);
    let item = Item::new(file, keywords)
        .ids(id.clone(), id)
        .user(user)
        .caption(title);
    Ok(crate::sites::events(crate::sites::post(
        &SITE,
        http,
        route,
        vec![item],
    )))
}

pub(crate) fn parse_sound(url: &str, page: &str) -> Map<String, Value> {
    let mut parts = url.trim_end_matches('/').rsplitn(3, '/');
    let slug = parts.next().unwrap_or_default();
    let user = parts.next().unwrap_or_default();
    let mut scan = crate::sites::art::Scan::new(page);
    let title = unescape(scan.extr("aria-label=\"title\">", "<"));
    let description = unescape(&remove_html(
        scan.extr("class=\"jp-description\">", "</div>"),
    ));
    let formats = scan.extr("\"setMedia\", {", "}");
    let mut data = Map::new();
    data.insert("user".into(), user.into());
    data.insert("slug".into(), slug.into());
    data.insert("title".into(), title.into());
    data.insert("description".into(), description.into());
    data.insert(
        "url".into(),
        extr(formats, ": \"", "\"").unwrap_or_default().into(),
    );
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let audio = Some(Target::Audio {
            user: "ClassWarAndPuppies".into(),
            slug: "687-Otto-von-Toontown-12822".into(),
        });
        assert_eq!(
            t("https://soundgasm.net/u/ClassWarAndPuppies/687-Otto-von-Toontown-12822"),
            audio
        );
        assert_eq!(
            t("https://www.soundgasm.net/user/ClassWarAndPuppies/687-Otto-von-Toontown-12822/x?y"),
            audio
        );
        assert_eq!(
            t("https://soundgasm.net/user/Demo"),
            Some(Target::User("Demo".into()))
        );
        assert_eq!(
            t("https://soundgasm.net/u/Demo/"),
            Some(Target::User("Demo".into()))
        );
        for raw in [
            "https://soundgasm.net/",
            "https://soundgasm.net/u/",
            "https://soundgasm.net/u/Demo?page=2",
            "https://soundgasm.net/users/Demo",
            "https://soundgasm.net.evil.test/u/Demo",
            "https://evilsoundgasm.net/u/Demo",
            "https://media.soundgasm.net/u/Demo",
            "https://soundgasm.net:8443/u/Demo",
            "https://u:p@soundgasm.net/u/Demo",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn clip(slug: &str) -> String {
        format!(
            r#"<div aria-label="title">Clip &amp; {slug}</div>
<div class="jp-description"><p>Line one</p>
<p>Two &amp; three</p></div>
<script>$(this).jPlayer("setMedia", {{ m4a: "https://media.soundgasm.net/sounds/{slug}.m4a" }});</script>"#
        )
    }

    #[tokio::test]
    async fn extracts_clip() {
        let (root, server) = serve(vec![html(&clip("abc"))]);
        let http = http("soundgasm", root);
        let files = media(extract(
            &http,
            Url::parse("https://soundgasm.net/user/Demo/Clip-1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://media.soundgasm.net/sounds/abc.m4a"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["user"], "Demo");
        assert_eq!(k["slug"], "Clip-1");
        assert_eq!(k["title"], "Clip & abc");
        assert_eq!(k["description"], "Line one Two & three");
        assert_eq!(k["filename"], "abc");
        assert_eq!(k["extension"], "m4a");
        assert_eq!(k["subcategory"], "audio");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Clip & abc.m4a");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("soundgasm/Demo")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{user}_{slug}",["Demo","Clip-1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /u/Demo/Clip-1 "));
    }

    #[tokio::test]
    async fn user_fetches_listed_clips_in_order() {
        let listing = r#"<div class="sound-details"><a href="https://soundgasm.net/u/Demo/one">One</a></div>
<div class="sound-details"><a href="https://evil.test/u/Demo/x">X</a></div>
<div class="sound-details"><a href="https://soundgasm.net/u/Demo/two">Two</a></div>"#;
        let (root, server) = serve(vec![html(listing), html(&clip("one")), html(&clip("two"))]);
        let http = http("soundgasm", root);
        let files = media(extract(
            &http,
            Url::parse("https://soundgasm.net/u/Demo").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["slug"], "one");
        assert_eq!(keywords(&files[1])["slug"], "two");
        assert_eq!(keywords(&files[1])["subcategory"], "user");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /user/Demo "));
        assert!(requests[1].starts_with("GET /u/Demo/one "));
        assert!(requests[2].starts_with("GET /u/Demo/two "));
    }
}
