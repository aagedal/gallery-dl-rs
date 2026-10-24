//! imgth galleries, mirroring gallery_dl/extractor/imgth.py: gallery pages
//! list thumbnails whose paths name the full-size images.
use super::Extr;
use crate::error;
use crate::sites::manga::{Gallery, Page};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "imgth",
    name: "imgth",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

const ROOT: &str = "https://imgth.com";

/// `imgth\.com/gallery/(\d+)`.
fn target(url: &Url) -> Option<String> {
    if !super::on_host(url, &["imgth.com"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/gallery/")?;
    let id = &rest[..rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len())];
    (!id.is_empty() && id.len() <= 20).then(|| id.to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported imgth URL"));
    };
    crate::sites::once(async move {
        let mut page = http
            .text(&http.api(&format!("{ROOT}/gallery/{id}/g/"))?)
            .await?;
        let data = metadata(&page, &id);
        let mut pages = Vec::new();
        let mut pnum = 0;
        loop {
            pages.extend(images(&page));
            if !page.contains("<li class=\"next\">") || pnum >= 10_000 {
                break;
            }
            pnum += 1;
            let url = http.api(&format!("{ROOT}/gallery/{id}/g/page/{pnum}"))?;
            page = http.text(&url).await?;
        }
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages).id(id)
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
}

fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut scan = Extr::new(page);
    let mut data = Map::new();
    data.insert("gallery_id".into(), super::parse_int(id).into());
    let title = crate::sites::unescape(scan.next("<h1>", "</h1>"));
    data.insert("title".into(), title.into());
    let count = scan.next("total of images in this gallery: ", " ");
    data.insert("count".into(), super::parse_int(count).into());
    let date = scan
        .next("created on ", " by <")
        .replacen("th, ", " ", 1)
        .replacen("nd, ", " ", 1)
        .replacen("st, ", " ", 1);
    let date = crate::sites::art::strptime(&date, "%B %d %Y at %H:%M");
    data.insert("date".into(), super::date_value(date));
    data.insert(
        "user".into(),
        crate::sites::unescape(scan.next(">", "<")).into(),
    );
    data
}

/// The full-size URLs of one page's thumbnails.
fn images(page: &str) -> Vec<Page> {
    let thumbs = crate::sites::art::extr(page, "<ul class=\"thumbnails\">", "</ul>");
    super::extract_iter(thumbs, "<img src=\"", "\"")
        .into_iter()
        .map(|url| {
            let path = super::partition(url, "/thumbs/").2;
            (format!("{ROOT}/images/{path}"), None)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imgth.com/gallery/37/wallpaper-anime").as_deref(),
            Some("37")
        );
        assert_eq!(t("http://www.imgth.com/gallery/12x").as_deref(), Some("12"));
        for raw in [
            "https://imgth.com/gallery/x",
            "https://img.imgth.com/gallery/1",
            "https://imgth.com.evil.test/gallery/1",
            "https://imgth.com:81/gallery/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn pages_until_no_next_link() {
        let first = "<h1>Anime &amp; More</h1>total of images in this gallery: 3 \
            created on October 21st, 2014 at 22:16 by <a href=\"/u/x\">Bob</a>\
            <ul class=\"thumbnails\"><li><img src=\"https://imgth.com/images/thumbs/2014/a.jpg\"></li>\
            <li><img src=\"https://imgth.com/images/thumbs/2014/b.png\"></li></ul><li class=\"next\">";
        let second = "<ul class=\"thumbnails\"><img src=\"https://imgth.com/images/thumbs/2014/c.jpg\">\
            <img src=\"https://imgth.com/images/thumbs/2014/d.jpg\"></ul>";
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("imgth", root);
        let files = media(extract(
            &http,
            Url::parse("https://imgth.com/gallery/37/x").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3, "count limits the images");
        assert_eq!(files[2].url.as_str(), "https://imgth.com/images/2014/c.jpg");
        let k = keywords(&files[1]);
        assert_eq!(k["gallery_id"], 37);
        assert_eq!(k["title"], "Anime & More");
        assert_eq!(k["count"], 3);
        assert_eq!(k["num"], 2);
        assert_eq!(k["date"], "2014-10-21 22:16:00");
        assert_eq!(k["user"], "Bob");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "imgth_37_002.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imgth/37 Anime & More")
        );
        assert_eq!(
            crate::sites::archive_key(&files[1]).unwrap().1,
            r#"["{gallery_id}_{num}",["37","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/37/g/ "));
        assert!(requests[1].starts_with("GET /gallery/37/g/page/1 "));
    }
}
