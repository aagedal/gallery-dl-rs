//! Madokami manga directories, mirroring gallery_dl/extractor/madokami.py.
//! Requires the `username` and `password` options (HTTP basic auth).
use super::set;
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, basic_auth, captures, date, group, nameext_into, parse_bytes, parse_int,
    parse_iso, re, split_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://manga.madokami.al";

static MANGA: Route = Route::new(
    "manga",
    "{filename}.{extension}",
    &["{category}", "{manga}"],
    "{chapter_id}",
);

pub static SITE: Site = Site {
    category: "madokami",
    name: "Madokami",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&MANGA],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://manga\.madokami\.al/Manga/(\w/\w{2}/\w{4}/.+)");
    captures(pattern, url).map(|caps| group(&caps, 1).to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(path) = target(&url) else {
        return crate::sites::fail(error("unsupported Madokami URL"));
    };
    crate::sites::once(async move {
        let (username, password) = auth_info(http)
            .ok_or_else(|| error("Madokami requires the username and password options"))?;
        let auth = basic_auth(&username, &password);
        let page = http
            .text_with(
                &http.api(&format!("{ROOT}/Manga/{path}"))?,
                &[("Authorization", &auth)],
            )
            .await?;
        let mut chapters = parse(&page, http.option_bool("chapter-reverse", false));
        let mut events = Vec::new();
        for chapter in chapters.drain(..) {
            let url = format!(
                "{ROOT}{}",
                crate::sites::gdl::str_of(&Value::Object(chapter.clone()), "path")
            );
            let Ok(file) = Url::parse(&url) else { continue };
            let mut data = chapter;
            nameext_into(file.as_str(), &mut data);
            let id = crate::sites::scalar(&data["chapter_id"]);
            let item = Item::new(file, data)
                .ids(id.clone(), id)
                .header("Authorization", &auth);
            events.extend(crate::sites::post(&SITE, http, &MANGA, vec![item]));
        }
        Ok(crate::sites::events(events))
    })
}

/// The chapter rows with the manga's shared metadata merged in.
fn parse(page: &str, reverse: bool) -> Vec<Map<String, Value>> {
    let mut scan = Scan::new(page);
    let mut chapters = Vec::new();
    while let Some(cid) = scan
        .find("<tr data-record=\"", "\"")
        .filter(|c| !c.is_empty())
    {
        let mut ch = Map::new();
        set(&mut ch, "chapter_id", parse_int(cid));
        set(&mut ch, "path", unescape(scan.extr("href=\"", "\"")));
        set(&mut ch, "chapter_string", unescape(scan.extr(">", "<")));
        set(&mut ch, "size", parse_bytes(scan.extr("<td>", "</td>")));
        set(
            &mut ch,
            "date",
            date(parse_iso(scan.extr("<td>", "</td>").trim())),
        );
        chapters.push(ch);
    }
    if reverse {
        chapters.reverse();
    }
    let mut shared = Map::new();
    set(
        &mut shared,
        "manga",
        unescape(scan.extr("itemprop=\"name\">", "<")),
    );
    set(
        &mut shared,
        "year",
        parse_int(scan.extr("itemprop=\"datePublished\" content=\"", "-")),
    );
    let author: Vec<String> = split_html(scan.extr("<p class=\"staff", "</p>"))
        .into_iter()
        .skip(1)
        .step_by(2)
        .collect();
    set(&mut shared, "author", author);
    set(
        &mut shared,
        "genre",
        split_html(scan.extr("<h3>Genres</h3>", "</div>")),
    );
    set(
        &mut shared,
        "tags",
        split_html(scan.extr("<h3>Tags</h3>", "</div>")),
    );
    set(
        &mut shared,
        "complete",
        scan.extr("span class=\"scanstatus\">", "<").to_lowercase() == "yes",
    );

    let full = re!(r"(?i)((?:v(?:ol)?\.?\s*(\d+))(?:\s+ch?\.?\s*(\d+)(?:-(\d+))?)?)");
    let short = re!(r"(?i)(ch?\.?\s*(\d+)(?:-(\d+))?)");
    for ch in &mut chapters {
        let chstr = ch["chapter_string"].as_str().unwrap_or_default().to_owned();
        let owned = |c: &regex::Captures<'_>, i: usize| group(c, i).to_owned();
        let parsed = full
            .captures(&chstr)
            .map(|c| (owned(&c, 1), owned(&c, 2), owned(&c, 3), owned(&c, 4)))
            .or_else(|| {
                short
                    .captures(&chstr)
                    .map(|c| (owned(&c, 1), String::new(), owned(&c, 2), owned(&c, 3)))
            });
        let (volume, chapter, end) = match parsed {
            Some((string, volume, chapter, end)) => {
                set(ch, "chapter_string", string);
                (volume, chapter, end)
            }
            None => Default::default(),
        };
        set(ch, "volume", parse_int(&volume));
        set(ch, "chapter", parse_int(&chapter));
        set(ch, "chapter_end", parse_int(&end));
        let mut merged = shared.clone();
        merged.extend(std::mem::take(ch));
        *ch = merged;
    }
    chapters
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{http, keywords, media, serve};

    const PAGE: &str = r#"<table><tr data-record="1001"><td><a href="/Manga/K/K_/K___/K/K%20v01%20c01-03.zip">K v01 c01-03 &amp; more.zip</a></td>
        <td>1.5M</td><td> 2013-08-01T12:00:00 </td></tr>
        <tr data-record="1002"><td><a href="/Manga/K/K_/K___/K/K%20ch.4.zip">K ch.4.zip</a></td><td>500k</td><td>2014-01-01</td></tr>
        <tr data-record="1003"><td><a href="/x/Extra.zip">Extra.zip</a></td><td>1k</td><td></td></tr></table>
        <span itemprop="name">K - Memory of Red</span><meta itemprop="datePublished" content="2013-02-01">
        <p class="staff"><a>Gorou</a><span>(Author)</span><a>Kuroe</a></p>
        <h3>Genres</h3><a>Action</a></div><h3>Tags</h3><a>Psychic</a></div><span class="scanstatus">Yes</span>"#;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://manga.madokami.al/Manga/K/K_/K___/K%20-%20Memory%20of%20Red").as_deref(),
            Some("K/K_/K___/K%20-%20Memory%20of%20Red")
        );
        assert_eq!(t("https://manga.madokami.al/Manga/K/K_/K__/x"), None);
    }

    #[test]
    fn parses_chapter_strings() {
        let chapters = parse(PAGE, false);
        assert_eq!(chapters.len(), 3);
        assert_eq!(chapters[0]["chapter_string"], "v01 c01-03");
        assert_eq!(chapters[0]["volume"], 1);
        assert_eq!(chapters[0]["chapter"], 1);
        assert_eq!(chapters[0]["chapter_end"], 3);
        assert_eq!(chapters[0]["size"], 1572864);
        assert_eq!(chapters[1]["chapter_string"], "ch.4");
        assert_eq!(chapters[1]["chapter"], 4);
        assert_eq!(chapters[2]["chapter_string"], "Extra.zip");
        assert_eq!(chapters[2]["volume"], 0);
        assert_eq!(chapters[0]["author"], serde_json::json!(["Gorou", "Kuroe"]));
        assert_eq!(chapters[0]["complete"], true);
        assert_eq!(chapters[0]["year"], 2013);
        assert_eq!(parse(PAGE, true)[0]["chapter_id"], 1003);
    }

    #[tokio::test]
    async fn requires_credentials_and_sends_basic_auth() {
        let url = Url::parse("https://manga.madokami.al/Manga/K/K_/K___/K").unwrap();
        let (root, server) = serve(vec![]);
        let anonymous = http("madokami", root.clone());
        assert!(crate::sites::testing::fails(extract(&anonymous, url.clone())).await);
        server.join().unwrap();

        let (root, server) = serve(vec![crate::sites::testing::html(PAGE)]);
        let options = serde_json::json!({"username": "user", "password": "pass"});
        let http = http("madokami", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(&http, url)).await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].headers["Authorization"], "Basic dXNlcjpwYXNz");
        assert_eq!(keywords(&files[0])["manga"], "K - Memory of Red");
        assert_eq!(path(&files[1]), "madokami/K - Memory of Red/K ch.4.zip");
        assert_eq!(archive(&files[1]), r#"["{chapter_id}",["1002"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /Manga/K/K_/K___/K "));
        assert!(
            requests[0].contains("authorization: Basic dXNlcjpwYXNz")
                || requests[0].contains("Authorization: Basic dXNlcjpwYXNz")
        );
    }
}
