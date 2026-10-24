//! Listal pictures, mirroring gallery_dl/extractor/listal.py: each image's
//! `viewimage` page names the file, its size, uploader and date.
use super::{date, path_segment};
use crate::sites::art::{Scan, extract_iter, nameext_into, parse_int, strptime};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.listal.com";
const FILENAME: &str = "{id}_{filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{title}"];
const ARCHIVE: &str = "{id}/{filename}";
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, ARCHIVE);
static PEOPLE: Route = Route::new("people", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "listal",
    name: "Listal",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &PEOPLE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Image(String),
    People(String),
}

/// `(?:www\.)?listal\.com/viewimage/(\d+)` and `/([^/?#]+)/pictures`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["listal.com"]) {
        return None;
    }
    let path = url.path();
    if let Some(id) = path
        .strip_prefix("/viewimage/")
        .and_then(crate::sites::art::leading_digits)
    {
        return Some(Target::Image(id.into()));
    }
    let (name, rest) = path[1..].split_once('/')?;
    (path_segment(name) && rest.starts_with("pictures")).then(|| Target::People(name.into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Image(id)) => images(http, &IMAGE, vec![id]),
        Some(Target::People(name)) => people(http, format!("{ROOT}/{name}/pictures")),
        None => crate::sites::fail(error("unsupported Listal URL")),
    }
}

/// `ListalPeopleExtractor.image_ids` over `_pagination`: page `N` lives at
/// `BASE/N`; a disabled `Next` span marks the last page.
fn people(http: &Http, base: String) -> Events<'_> {
    crate::sites::paginate(Some(1u32), move |number| {
        let base = base.clone();
        async move {
            let Some(number) = number else {
                return Ok(None);
            };
            let url = match number {
                1 => base.clone(),
                n => format!("{base}/{n}"),
            };
            let page = http.text(&http.api(&url)?).await?;
            let ids: Vec<String> = extract_iter(&page, "listal.com/viewimage/", "'")
                .into_iter()
                .filter(|id| path_segment(id))
                .map(str::to_owned)
                .collect();
            // A page without images would otherwise be followed forever.
            let last = page.contains("<span class='nextprev'>Next") || ids.is_empty();
            let next = (!last && number < 100_000).then_some(number + 1);
            Ok(Some((images(http, &PEOPLE, ids), next)))
        }
    })
}

/// One post per image, each page requested as the consumer reaches it.
fn images<'a>(http: &'a Http, route: &'static Route, ids: Vec<String>) -> Events<'a> {
    crate::sites::pages(futures_util::stream::iter(ids).then(move |id| async move {
        let page = http
            .text(&http.api(&format!("{ROOT}/viewimage/{id}h"))?)
            .await?;
        let item = image(&page, &id)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            route,
            vec![item],
        )))
    }))
}

/// `_extract_image`.
fn image(page: &str, id: &str) -> Result<Item> {
    let mut scan = Scan::new(page);
    let raw = scan.extr("<div><center><img src='", "'");
    let mut data = Map::new();
    data.insert("id".into(), id.into());
    data.insert("url".into(), raw.into());
    let unescape = crate::sites::unescape;
    data.insert("title".into(), unescape(scan.extr("title=\"", "\"")).into());
    data.insert(
        "width".into(),
        parse_int(scan.extr("width='", "'"), 0).into(),
    );
    data.insert(
        "height".into(),
        parse_int(scan.extr("height='", "'"), 0).into(),
    );
    data.insert(
        "author_url".into(),
        scan.extr("Added by <a href='", "'").into(),
    );
    let author = unescape(scan.extr(">", "<"));
    data.insert("author".into(), author.as_str().into());
    let ts = strptime(scan.extr(" ago on ", "<"), "%d %B %Y %H:%M");
    data.insert("date".into(), date(ts));
    nameext_into(raw, &mut data);
    let Some(url) = crate::sites::media_url(raw, &["lisimg.com", "listal.com"]) else {
        return Err(error("Listal image page has no file URL"));
    };
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    Ok(Item::new(url, data)
        .ids(id, id)
        .user(Some(author))
        .caption(title)
        .timestamp(ts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.listal.com/viewimage/29620846"),
            Some(Target::Image("29620846".into()))
        );
        assert_eq!(
            t("https://listal.com/adam-sandler/pictures/2"),
            Some(Target::People("adam-sandler".into()))
        );
        assert_eq!(
            t("https://www.listal.com/viewimage/pictures"),
            Some(Target::People("viewimage".into()))
        );
        for raw in [
            "https://www.listal.com/",
            "https://www.listal.com/adam-sandler",
            "https://www.listal.com/viewimage/",
            "https://lthumb.listal.com/viewimage/1",
            "https://www.listal.com.evil.test/viewimage/1",
            "https://evillistal.com/viewimage/1",
            "https://www.listal.com:8080/viewimage/1",
            "https://u:p@www.listal.com/viewimage/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn image_page(id: u32) -> String {
        format!(
            "<div><center><img src='https://lthumb.lisimg.com/{id}/full.jpg?x' title=\"Adam &amp; Co\" \
             width='800' height='600'></center></div> Added by <a href='https://www.listal.com/user'>Us&amp;er</a> \
             3 years ago on 12 March 2020 10:15<br>"
        )
    }

    #[tokio::test]
    async fn people_pages_until_last() {
        let first = "<a href='https://www.listal.com/viewimage/11'><a href='https://www.listal.com/viewimage/12'>";
        let second = "<a href='https://www.listal.com/viewimage/13'><span class='nextprev'>Next &#187;</span>";
        let (root, server) = serve(vec![
            html(first),
            html(&image_page(11)),
            html(&image_page(12)),
            html(second),
            html(&image_page(13)),
        ]);
        let http = http("listal", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.listal.com/adam-sandler/pictures").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://lthumb.lisimg.com/11/full.jpg?x"
        );
        let k = keywords(&files[2]);
        assert_eq!(k["id"], "13");
        assert_eq!(k["title"], "Adam & Co");
        assert_eq!((&k["width"], &k["height"]), (&800.into(), &600.into()));
        assert_eq!(k["author"], "Us&er");
        assert_eq!(k["author_url"], "https://www.listal.com/user");
        assert_eq!(k["date"], "2020-03-12 10:15:00");
        assert_eq!(k["subcategory"], "people");
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "13_full.jpg");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("listal/Adam & Co")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}/{filename}",["11","full"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /adam-sandler/pictures "));
        assert!(requests[1].starts_with("GET /viewimage/11h "));
        assert!(requests[3].starts_with("GET /adam-sandler/pictures/2 "));
        assert!(requests[4].starts_with("GET /viewimage/13h "));
    }

    #[tokio::test]
    async fn single_image() {
        let (root, server) = serve(vec![html(&image_page(5))]);
        let http = http("listal", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.listal.com/viewimage/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "image");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /viewimage/5h "));
    }
}
