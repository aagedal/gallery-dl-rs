//! Adult Empire galleries, mirroring gallery_dl/extractor/adultempire.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extract_iter, group, parse_int, re, split_html, strptime, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://www.adultempire.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "adultempire",
    name: "Adult Empire",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: Some("adultempire.com"),
};

fn target(url: &Url) -> Option<(String, String)> {
    let c = captures(
        re!(r"^https?://(?:www\.)?adult(?:dvd)?empire\.com(/(\d+)/gallery\.html)"),
        url,
    )?;
    Some((group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, id)) = target(&url) else {
        return crate::sites::fail(error("unsupported Adult Empire URL"));
    };
    crate::sites::once(async move {
        let base = http.api(ROOT)?;
        http.set_cookie(
            &base,
            "ageConfirmed=true; Domain=www.adultempire.com; Path=/",
        );
        let page_url = format!("{ROOT}{path}");
        let mut page = http.text(&http.api(&page_url)?).await?;
        let start = page
            .find("<div id=\"content\">")
            .ok_or_else(|| error("Adult Empire gallery has no content"))?;
        let mut scan = Scan::at(&page, start);
        let mut data = Map::new();
        data.insert("gallery_id".into(), parse_int(&id).into());
        data.insert("title".into(), unescape(scan.extr("title=\"", "\"")).into());
        data.insert(
            "studio".into(),
            scan.extr(">studio</small>", "<").trim().into(),
        );
        data.insert(
            "date".into(),
            date(strptime(
                scan.extr(">released</small>", "<").trim(),
                "%m/%d/%Y",
            )),
        );
        let mut actors: Vec<String> =
            split_html(scan.extr("<ul class=\"item-details item-cast-list ", "</ul>"))
                .into_iter()
                .skip(1)
                .collect();
        actors.sort();
        data.insert("actors".into(), actors.into());
        let mut images = Vec::new();
        let mut number = 1;
        loop {
            let urls: Vec<String> = extract_iter(&page, "rel=\"L\"><img src=\"", "\"")
                .into_iter()
                .map(|u| u.replace("_200.", "_9600."))
                .collect();
            let full = urls.len() >= 24;
            images.extend(urls.into_iter().map(|u| (u, None)));
            if !full {
                break;
            }
            number += 1;
            let mut next = http.api(&page_url)?;
            next.query_pairs_mut()
                .append_pair("page", &number.to_string());
            page = http.text(&next).await?;
        }
        Ok(crate::sites::events(super::numbered(
            &SITE, http, &GALLERY, &data, images, &id,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        assert_eq!(
            target(&Url::parse("https://www.adultdvdempire.com/5683/gallery.html").unwrap()),
            Some(("/5683/gallery.html".into(), "5683".into()))
        );
        assert_eq!(
            target(&Url::parse("https://www.adultempire.com/5683/movie.html").unwrap()),
            None
        );
    }

    #[tokio::test]
    async fn extracts_full_size_images() {
        let page = r#"<div id="content"><h1 title="Movie &amp; Title"></h1><small>studio</small> Studio X </li>
            <small>released</small> 01/02/2010 </li><ul class="item-details item-cast-list x"><li><a>Bob</a></li><li><a>Ann</a></li></ul>
            <a rel="L"><img src="https://imgs1cdn.adultempire.com/gw/1/a_200.jpg"></a>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("adultempire", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.adultempire.com/5683/gallery.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://imgs1cdn.adultempire.com/gw/1/a_9600.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["actors"], serde_json::json!(["Ann", "Bob"]));
        assert_eq!(k["date"], "2010-01-02 00:00:00");
        assert_eq!(k["studio"], "Studio X");
        server.join().unwrap();
    }
}
