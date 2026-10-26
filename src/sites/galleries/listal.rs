//! Listal images and people pictures, mirroring gallery_dl/extractor/listal.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extract_iter, group, nameext_into, parse_int, re, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://www.listal.com";
const BASE: &str = r"^https?://(?:www\.)?listal\.com";

static IMAGE: Route = Route::new(
    "image",
    "{id}_{filename}.{extension}",
    &["{category}", "{title}"],
    "{id}/{filename}",
);
static PEOPLE: Route = Route::new(
    "people",
    "{id}_{filename}.{extension}",
    &["{category}", "{title}"],
    "{id}/{filename}",
);

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

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/viewimage/(\d+)")), url) {
        return Some(Target::Image(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/([^/?#]+)/pictures")), url)
        .map(|c| Target::People(group(&c, 1).into()))
}

/// `_extract_image`.
async fn image(http: &Http, image_id: &str) -> crate::Result<Option<Item>> {
    let page = http
        .text(&http.api(&format!("{ROOT}/viewimage/{image_id}h"))?)
        .await?;
    let mut scan = Scan::new(&page);
    let mut img = Map::new();
    img.insert("id".into(), image_id.into());
    let url = scan.extr("<div><center><img src='", "'").to_owned();
    img.insert("url".into(), url.as_str().into());
    img.insert("title".into(), unescape(scan.extr("title=\"", "\"")).into());
    img.insert("width".into(), parse_int(scan.extr("width='", "'")).into());
    img.insert(
        "height".into(),
        parse_int(scan.extr("height='", "'")).into(),
    );
    img.insert(
        "author_url".into(),
        scan.extr("Added by <a href='", "'").into(),
    );
    img.insert("author".into(), unescape(scan.extr(">", "<")).into());
    img.insert(
        "date".into(),
        date(strptime(scan.extr(" ago on ", "<"), "%d %B %Y %H:%M")),
    );
    nameext_into(&url, &mut img);
    Ok(Url::parse(&url)
        .ok()
        .map(|file| Item::new(file, img).ids(image_id, image_id)))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Listal URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let items = image(http, &id).await?.into_iter().collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
        Target::People(name) => crate::sites::paginate(Some(1u64), move |state| {
            let name = name.clone();
            async move {
                let Some(number) = state else { return Ok(None) };
                let base = format!("{ROOT}/{name}/pictures");
                let url = if number == 1 {
                    base
                } else {
                    format!("{base}/{number}")
                };
                let page = http.text(&http.api(&url)?).await?;
                let mut events = Vec::new();
                for image_id in extract_iter(&page, "listal.com/viewimage/", "'") {
                    events.extend(crate::sites::post(
                        &SITE,
                        http,
                        &PEOPLE,
                        image(http, image_id).await?.into_iter().collect(),
                    ));
                }
                let next = page
                    .contains("<span class='nextprev'>Next")
                    .then_some(number + 1);
                Ok(Some((crate::sites::events(events), next)))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[tokio::test]
    async fn extracts_image_page() {
        let page = "<div><center><img src='https://lthumb.lisimg.com/1/2/x.jpg' title=\"Jane &amp; Co\" width='800' height='600'>
            Added by <a href='https://www.listal.com/list/u'>User</a> 3 years ago on 4 May 2021 12:34<br>";
        let (root, server) = serve(vec![html(page)]);
        let http = client("listal", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.listal.com/viewimage/29620846").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["author"].as_str(),
                k["width"].as_i64()
            ),
            (Some("Jane & Co"), Some("User"), Some(800))
        );
        assert_eq!(k["date"], "2021-05-04 12:34:00");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "29620846_x.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /viewimage/29620846h "));
    }
}
