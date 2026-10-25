//! Adult Empire scene galleries, mirroring
//! gallery_dl/extractor/adultempire.py: the `ageConfirmed` cookie is set,
//! and gallery pages of 24 thumbnails are read until a shorter one, each
//! thumbnail's `_200.` size swapped for `_9600.`.
use crate::error;
use crate::sites::art::{
    Scan, date, extract_iter, leading_digits, on_host, parse_int, split_html, strptime,
};
use crate::sites::manga::{Gallery, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use url::Url;

const ROOT: &str = "https://www.adultempire.com";
/// Gallery pages read at most, a bound on the open-ended pagination.
const MAX_PAGES: usize = 500;

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
    cookies: Some("www.adultempire.com"),
};

/// `adult(?:dvd)?empire\.com(/(\d+)/gallery\.html)`: the gallery ID.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["adultempire.com", "adultdvdempire.com"]) {
        return None;
    }
    let rest = url.path().strip_prefix('/')?;
    let id = leading_digits(rest)?;
    rest[id.len()..]
        .starts_with("/gallery.html")
        .then(|| id.to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported Adult Empire URL"));
    };
    crate::sites::once(async move {
        let page_url = http.api(&format!("{ROOT}/{id}/gallery.html"))?;
        http.set_cookie(&page_url, "ageConfirmed=true; Path=/");
        let mut page = http.text(&page_url).await?;
        let start = page
            .find("<div id=\"content\">")
            .ok_or_else(|| error("Adult Empire gallery page has no content"))?;
        let mut scan = Scan::new(&page[start..]);
        let title = unescape(scan.extr("title=\"", "\""));
        let studio = scan.extr(">studio</small>", "<").trim().to_owned();
        let released = strptime(scan.extr(">released</small>", "<").trim(), "%m/%d/%Y");
        let mut actors: Vec<String> =
            split_html(scan.extr("<ul class=\"item-details item-cast-list ", "</ul>"))
                .into_iter()
                .skip(1)
                .collect();
        actors.sort();
        let data = keywords([
            ("gallery_id", parse_int(&id, 0).into()),
            ("title", title.into()),
            ("studio", studio.into()),
            ("date", date(released)),
            ("actors", actors.into()),
        ]);
        let mut pages = Vec::new();
        for number in 2..=MAX_PAGES + 1 {
            let urls = extract_iter(&page, "rel=\"L\"><img src=\"", "\"");
            let full = urls.len() >= 24;
            pages.extend(
                urls.into_iter()
                    .map(|url| (url.replace("_200.", "_9600."), None)),
            );
            if !full || number > MAX_PAGES {
                break;
            }
            let mut next = page_url.clone();
            next.query_pairs_mut()
                .append_pair("page", &number.to_string());
            page = http.text(&next).await?;
        }
        // The images are a generator upstream, so `count` stays unset.
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages)
                .id(id)
                .timestamp(released)
                .uncounted()
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.adultempire.com/5998/gallery.html"),
            Some("5998".into())
        );
        assert_eq!(
            t("https://adultdvdempire.com/5998/gallery.html?x"),
            Some("5998".into())
        );
        for raw in [
            "https://www.adultempire.com/5998/movie.html",
            "https://www.adultempire.com/gallery.html",
            "https://www.adultempire.com.evil.test/1/gallery.html",
            "https://www.adultempire.com:8080/1/gallery.html",
            "https://u:p@www.adultempire.com/1/gallery.html",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn page(count: usize, offset: usize) -> String {
        let thumbs: String = (0..count)
            .map(|i| {
                format!(
                    "<a rel=\"L\"><img src=\"https://imgs.ae.test/g/{}_200.jpg\"></a>",
                    offset + i
                )
            })
            .collect();
        format!(
            "<div id=\"content\"><h1 title=\"Scene &amp; Title\">\
<small>studio</small> Studio One <small>released</small> 02/03/2020 </div>\
<ul class=\"item-details item-cast-list x\"><li>Zed</li><li>Amy</li></ul>{thumbs}"
        )
    }

    #[tokio::test]
    async fn paginates_gallery() {
        let (root, server) = serve(vec![html(&page(24, 0)), html(&page(1, 24))]);
        let http = http("adultempire", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.adultempire.com/5998/gallery.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 25);
        assert_eq!(files[24].url.as_str(), "https://imgs.ae.test/g/24_9600.jpg");
        let k = kw(&files[24]);
        assert_eq!(k["gallery_id"], 5998);
        assert_eq!(k["title"], "Scene & Title");
        assert_eq!(k["studio"], "Studio One");
        assert_eq!(k["date"], "2020-02-03 00:00:00");
        assert_eq!(k["actors"], json!(["Amy", "Zed"]));
        assert_eq!(k["num"], 25);
        assert!(!k.contains_key("count"));
        assert_eq!(
            path(&files[24]),
            "adultempire/5998 Scene & Title/adultempire_5998_025.jpg"
        );
        assert_eq!(archive(&files[0]), r#"["{gallery_id}_{num}",["5998","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /5998/gallery.html "));
        assert!(requests[0].contains("cookie: ageConfirmed=true"));
        assert!(requests[1].starts_with("GET /5998/gallery.html?page=2 "));
    }
}
