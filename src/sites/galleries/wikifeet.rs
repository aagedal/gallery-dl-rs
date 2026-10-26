//! wikiFeet (and wikiFeetX, men.wikifeet) celebrity galleries, mirroring
//! gallery_dl/extractor/wikifeet.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, group, parse_float, parse_iso, re, root_of, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{celeb}_{pid}.{extension}",
    &["{category}", "{celebrity}"],
    "{type}_{celeb}_{pid}",
);

pub static WIKIFEET: Site = Site {
    category: "wikifeet",
    name: "wikiFeet",
    matches: |url| target(url).is_some_and(|(root, _)| !root.contains("wikifeetx.com")),
    extract: |http, url| extract(&WIKIFEET, http, url),
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

pub static WIKIFEETX: Site = Site {
    category: "wikifeetx",
    name: "wikiFeetX",
    matches: |url| target(url).is_some_and(|(root, _)| root.contains("wikifeetx.com")),
    extract: |http, url| extract(&WIKIFEETX, http, url),
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<(String, String)> {
    captures(
        re!(r"^https?://(?:(?:www\.)?wikifeetx?|men\.wikifeet)\.com/([^/?#]+)"),
        url,
    )
    .map(|c| (root_of(url), group(&c, 1).into()))
}

fn tag(code: &str) -> Option<&'static str> {
    Some(match code {
        "C" => "Close-up",
        "T" => "Toenails",
        "N" => "Nylons",
        "A" => "Arches",
        "S" => "Soles",
        "B" => "Barefoot",
        _ => return None,
    })
}

fn extract<'a>(site: &'static Site, http: &'a Http, url: Url) -> Events<'a> {
    let Some((root, celeb)) = target(&url) else {
        return crate::sites::fail(error("unsupported wikiFeet URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{root}/{celeb}"))?).await?;
        let mut scan = Scan::new(&page);
        let mut data = Map::new();
        data.insert("celeb".into(), celeb.as_str().into());
        data.insert(
            "type".into(),
            if root.contains("://men.") {
                "men"
            } else {
                "women"
            }
            .into(),
        );
        data.insert(
            "birthplace".into(),
            unescape(scan.extr("\"bplace\":\"", "\"")).into(),
        );
        let birthday = unescape(scan.extr("\"bdate\":\"", "\""));
        data.insert(
            "birthday".into(),
            crate::sites::gdl::date(parse_iso(birthday.get(..10).unwrap_or(&birthday))),
        );
        data.insert(
            "shoesize".into(),
            unescape(scan.extr("\"ssize\":", ",")).into(),
        );
        data.insert(
            "rating".into(),
            parse_float(scan.extr("\"score\":", ",")).into(),
        );
        data.insert(
            "celebrity".into(),
            unescape(scan.extr("\"cname\":\"", "\"")).into(),
        );
        let gallery: Vec<Value> =
            serde_json::from_str(&format!("[{}]", extr(&page, "\"gallery\":[", "],")))
                .map_err(|_| error("wikiFeet gallery data is invalid"))?;
        let images = gallery
            .iter()
            .map(|entry| {
                let pid = crate::sites::scalar(&entry["pid"]);
                let tags: Vec<Value> = entry["tags"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .filter_map(|c| tag(&c.to_string()))
                    .map(Value::from)
                    .collect();
                let mut image = Map::new();
                image.insert("pid".into(), entry["pid"].clone());
                image.insert("width".into(), entry["pw"].clone());
                image.insert("height".into(), entry["ph"].clone());
                image.insert("tags".into(), tags.into());
                (
                    format!("https://pics.wikifeet.com/{celeb}-Feet-{pid}.jpg"),
                    Some(image),
                )
            })
            .collect();
        Ok(crate::sites::events(super::numbered(
            site, http, &GALLERY, &data, images, &celeb,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn picks_site_by_host() {
        let u = |raw: &str| Url::parse(raw).unwrap();
        assert!((WIKIFEET.matches)(&u(
            "https://men.wikifeet.com/Chris_Hemsworth"
        )));
        assert!(!(WIKIFEET.matches)(&u(
            "https://www.wikifeetx.com/Tifa_Quinn"
        )));
        assert!((WIKIFEETX.matches)(&u(
            "https://www.wikifeetx.com/Tifa_Quinn"
        )));
    }

    #[tokio::test]
    async fn extracts_gallery_json() {
        let page = r#"{"bplace":"USA","bdate":"1990-05-06T00:00:00","ssize":9,"score":4.5,"cname":"Jane Doe","gallery":[{"pid":11,"pw":800,"ph":600,"tags":"SBX"}],"other":1}"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("wikifeet", root);
        let files = media(extract(
            &WIKIFEET,
            &http,
            Url::parse("https://www.wikifeet.com/Jane_Doe").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://pics.wikifeet.com/Jane_Doe-Feet-11.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], serde_json::json!(["Soles", "Barefoot"]));
        assert_eq!(k["type"], "women");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "wikifeet_Jane_Doe_11.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("wikifeet/Jane Doe")
        );
        server.join().unwrap();
    }
}
