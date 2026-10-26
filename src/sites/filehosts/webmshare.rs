//! webmshare videos, mirroring gallery_dl/extractor/webmshare.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, group, parse_int, re, rpartition, strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://webmshare.com";

static VIDEO: Route = Route::new(
    "video",
    "{id}{title:? //}.{extension}",
    &["{category}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "webmshare",
    name: "webmshare",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&VIDEO],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://(?:s\d+\.)?webmshare\.com/(?:play/|download-webm/)?(\w{3,})");
    captures(pattern, url).map(|c| group(&c, 1).to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported webmshare URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}/{id}"))?).await?;
        let mut scan = Scan::new(&page);
        let mut title =
            unescape(rpartition(scan.extr("property=\"og:title\" content=\"", "\""), " — ").0);
        if title == "webmshare" {
            title.clear();
        }
        let thumb = format!(
            "https:{}",
            scan.extr("property=\"og:image\" content=\"", "\"")
        );
        let video = format!(
            "https:{}",
            scan.extr("property=\"og:video\" content=\"", "\"")
        );
        let mut data = Map::new();
        data.insert("title".into(), title.into());
        data.insert("thumb".into(), thumb.into());
        data.insert("url".into(), video.as_str().into());
        data.insert(
            "width".into(),
            parse_int(scan.extr("property=\"og:video:width\" content=\"", "\"")).into(),
        );
        data.insert(
            "height".into(),
            parse_int(scan.extr("property=\"og:video:height\" content=\"", "\"")).into(),
        );
        data.insert(
            "date".into(),
            date(strptime(scan.extr("<small>Added ", "<"), "%B %d, %Y")),
        );
        data.insert(
            "views".into(),
            parse_int(scan.extr("glyphicon-eye-open\"></span>", "<")).into(),
        );
        data.insert("id".into(), id.as_str().into());
        data.insert("filename".into(), id.as_str().into());
        data.insert("extension".into(), "webm".into());
        let file = Url::parse(&video).map_err(|_| error("webmshare video has no URL"))?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &VIDEO,
            vec![Item::new(file, data).ids(id.clone(), id)],
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
        assert_eq!(t("https://webmshare.com/O9mWY").as_deref(), Some("O9mWY"));
        assert_eq!(
            t("https://s1.webmshare.com/zBGAg.webm").as_deref(),
            Some("zBGAg")
        );
        assert_eq!(
            t("https://webmshare.com/download-webm/zBGAg").as_deref(),
            Some("zBGAg")
        );
        assert_eq!(t("https://webmshare.com/ab"), None);
    }

    #[tokio::test]
    async fn extracts_video() {
        let page = r#"<meta property="og:title" content="Cat &amp; Dog — webmshare"><meta property="og:image" content="//s1.webmshare.com/t/O9mWY.jpg">
            <meta property="og:video" content="//s1.webmshare.com/O9mWY.webm"><meta property="og:video:width" content="640">
            <meta property="og:video:height" content="360"><small>Added August 08, 2018</small><span class="glyphicon-eye-open"></span>42</span>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("webmshare", root);
        let files = media(extract(
            &http,
            Url::parse("https://webmshare.com/play/O9mWY").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://s1.webmshare.com/O9mWY.webm");
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Cat & Dog");
        assert_eq!(k["date"], "2018-08-08 00:00:00");
        assert_eq!(k["views"], 42);
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "O9mWY Cat & Dog.webm");
        server.join().unwrap();
    }
}
