//! imgth galleries, mirroring gallery_dl/extractor/imgth.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, parse_int, partition, re, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://imgth.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "imgth",
    name: "imgth",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://(?:www\.)?imgth\.com/gallery/(\d+)");
    captures(pattern, url).map(|c| group(&c, 1).to_owned())
}

fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    data.insert("gallery_id".into(), parse_int(id).into());
    data.insert("title".into(), unescape(scan.extr("<h1>", "</h1>")).into());
    data.insert(
        "count".into(),
        parse_int(scan.extr("total of images in this gallery: ", " ")).into(),
    );
    let created = scan
        .extr("created on ", " by <")
        .replacen("th, ", " ", 1)
        .replacen("nd, ", " ", 1)
        .replacen("st, ", " ", 1);
    data.insert("date".into(), date(strptime(&created, "%B %d %Y at %H:%M")));
    data.insert("user".into(), unescape(scan.extr(">", "<")).into());
    data
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
        let count = data["count"].as_i64().unwrap_or(0).max(0) as usize;
        let mut urls = Vec::new();
        let mut number = 0;
        loop {
            let thumbs = extr(&page, "<ul class=\"thumbnails\">", "</ul>");
            for thumb in extract_iter(thumbs, "<img src=\"", "\"") {
                urls.push(format!("{ROOT}/images/{}", partition(thumb, "/thumbs/").2));
            }
            if urls.len() >= count || !page.contains("<li class=\"next\">") {
                break;
            }
            number += 1;
            page = http
                .text(&http.api(&format!("{ROOT}/gallery/{id}/g/page/{number}"))?)
                .await?;
        }
        let items = urls
            .into_iter()
            .take(count)
            .enumerate()
            .filter_map(|(index, raw)| {
                let url = Url::parse(&raw).ok()?;
                let mut fields = data.clone();
                fields.insert("num".into(), (index + 1).into());
                crate::sites::gdl::nameext_into(&raw, &mut fields);
                Some(Item::new(url, fields).ids(id.clone(), (index + 1).to_string()))
            })
            .collect();
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &GALLERY, items,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.imgth.com/gallery/37/wallpaper-anime").as_deref(),
            Some("37")
        );
        assert_eq!(t("https://imgth.com/image/37"), None);
    }

    #[tokio::test]
    async fn pages_through_thumbnails() {
        let first = r#"<h1>Wallpaper &amp; Anime</h1>total of images in this gallery: 3 created on June 20th, 2013 at 14:55 by <a>user</a>
            <ul class="thumbnails"><li><img src="https://imgth.com/images/thumbs/2013/06/a.jpg"><img src="https://imgth.com/images/thumbs/2013/06/b.png"></ul><li class="next">"#;
        let second = r#"<ul class="thumbnails"><img src="https://imgth.com/images/thumbs/2013/06/c.jpg"></ul><li class="next">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("imgth", root);
        let files = media(extract(
            &http,
            Url::parse("https://imgth.com/gallery/37/x").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[1].url.as_str(),
            "https://imgth.com/images/2013/06/b.png"
        );
        let k = keywords(&files[2]);
        assert_eq!(k["title"], "Wallpaper & Anime");
        assert_eq!(k["date"], "2013-06-20 14:55:00");
        assert_eq!(k["user"], "user");
        assert_eq!(k["num"], 3);
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "imgth_37_003.jpg");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("imgth/37 Wallpaper & Anime")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/37/g/ "));
        assert!(requests[1].starts_with("GET /gallery/37/g/page/1 "));
    }
}
