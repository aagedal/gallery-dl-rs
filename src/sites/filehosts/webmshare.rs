//! webmshare videos, mirroring gallery_dl/extractor/webmshare.py: the video
//! page's Open Graph tags name the file.
use super::Extr;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "webmshare",
    name: "webmshare",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&VIDEO],
    api_key: false,
    cookies: None,
};

static VIDEO: Route = Route::new(
    "video",
    "{id}{title:? //}.{extension}",
    &["{category}"],
    "{id}",
);

const ROOT: &str = "https://webmshare.com";

/// The leading `\w` run of `text`.
fn word(text: &str) -> &str {
    let len = text
        .bytes()
        .position(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
        .unwrap_or(text.len());
    &text[..len]
}

/// `(?:s\d+\.)?webmshare\.com/(?:play/|download-webm/)?(\w{3,})`.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) {
        return None;
    }
    let sub = url.host_str()?.strip_suffix("webmshare.com")?;
    let server = sub
        .strip_prefix('s')
        .and_then(|n| n.strip_suffix('.'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !sub.is_empty() && !server {
        return None;
    }
    let path = &url.path()[1..];
    ["play/", "download-webm/", ""]
        .into_iter()
        .filter_map(|prefix| path.strip_prefix(prefix).map(word))
        .find(|id| id.len() >= 3)
        .map(str::to_owned)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported webmshare URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}/{id}"))?).await?;
        let item = parse(&page, &id)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &VIDEO,
            vec![item],
        )))
    })
}

fn parse(page: &str, id: &str) -> Result<Item> {
    let mut extr = Extr::new(page);
    let og = |extr: &mut Extr, name: &str| {
        extr.next(&format!("property=\"og:{name}\" content=\""), "\"")
            .to_owned()
    };
    let title = og(&mut extr, "title");
    let mut title = crate::sites::unescape(super::rpartition(&title, " — ").0);
    let thumb = format!("https:{}", og(&mut extr, "image"));
    let url = format!("https:{}", og(&mut extr, "video"));
    let width = super::parse_int(&og(&mut extr, "video:width"));
    let height = super::parse_int(&og(&mut extr, "video:height"));
    let date = super::strptime(extr.next("<small>Added ", "<"), "%B %d, %Y");
    let views = super::parse_int(extr.next("glyphicon-eye-open\"></span>", "<"));
    if title == "webmshare" {
        title.clear();
    }
    let mut data = Map::new();
    data.insert("title".into(), title.clone().into());
    data.insert("thumb".into(), thumb.into());
    data.insert("url".into(), url.clone().into());
    data.insert("width".into(), width.into());
    data.insert("height".into(), height.into());
    data.insert("date".into(), super::date_value(date));
    data.insert("views".into(), views.into());
    data.insert("id".into(), id.into());
    data.insert("filename".into(), id.into());
    data.insert("extension".into(), Value::from("webm"));
    let file = super::web_url(&url).ok_or_else(|| error("webmshare video has no file"))?;
    Ok(Item::new(file, data)
        .ids(id, id)
        .caption(Some(title))
        .timestamp(date))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(t("https://webmshare.com/O9mWY").as_deref(), Some("O9mWY"));
        assert_eq!(
            t("https://s1.webmshare.com/play/O9mWY").as_deref(),
            Some("O9mWY")
        );
        assert_eq!(
            t("https://webmshare.com/download-webm/8K5Pz").as_deref(),
            Some("8K5Pz")
        );
        assert_eq!(t("https://webmshare.com/play/ab").as_deref(), Some("play"));
        for raw in [
            "https://webmshare.com/ab",
            "https://www.webmshare.com/O9mWY",
            "https://s.webmshare.com/O9mWY",
            "https://evilwebmshare.com/O9mWY",
            "https://webmshare.com:81/O9mWY",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_video() {
        let page = "<meta property=\"og:title\" content=\"Cat &amp; dog — webmshare\">\
            <meta property=\"og:image\" content=\"//s1.webmshare.com/t/O9mWY.jpg\">\
            <meta property=\"og:video\" content=\"//s1.webmshare.com/O9mWY.webm\">\
            <meta property=\"og:video:width\" content=\"640\">\
            <meta property=\"og:video:height\" content=\"360\">\
            <small>Added June 05, 2024</small><span class=\"glyphicon glyphicon-eye-open\"></span>42<";
        let (root, server) = serve(vec![html(page)]);
        let http = http("webmshare", root);
        let files = media(extract(
            &http,
            Url::parse("https://webmshare.com/play/O9mWY").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://s1.webmshare.com/O9mWY.webm");
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Cat & dog");
        assert_eq!(k["thumb"], "https://s1.webmshare.com/t/O9mWY.jpg");
        assert_eq!(k["width"], 640);
        assert_eq!(k["date"], "2024-06-05 00:00:00");
        assert_eq!(k["views"], 42);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "O9mWY Cat & dog.webm");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("webmshare")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["O9mWY"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /O9mWY "));
    }

    #[test]
    fn default_title_is_blank() {
        let page = "<meta property=\"og:title\" content=\"webmshare — webmshare\">\
            <meta property=\"og:video\" content=\"//s1.webmshare.com/x.webm\">";
        let item = parse(page, "abc").unwrap();
        assert_eq!(item.keywords["title"], "");
        assert!(parse("<p>gone</p>", "abc").is_err());
    }
}
