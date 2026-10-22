//! SlideShare presentations, mirroring gallery_dl/extractor/slideshare.py
//! through the page's `__NEXT_DATA__` slideshow description.
use super::{extr, on_host, raw_segments, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static PRESENTATION: Route = Route::new(
    "presentation",
    "{presentation}-{num:>02}.{extension}",
    &["{category}", "{user}"],
    "{presentation}_{num}",
);

pub static SITE: Site = Site {
    category: "slideshare",
    name: "SlideShare",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&PRESENTATION],
    api_key: false,
};

/// `slideshare\.net/(?:mobile/)?([^/?#]+)/([^/?#]+)`.
fn target(url: &Url) -> Option<(String, String)> {
    if !on_host(url, &["slideshare.net"]) {
        return None;
    }
    let parts = raw_segments(url);
    let start = usize::from(parts.len() >= 3 && parts[0] == "mobile");
    let (user, presentation) = (*parts.get(start)?, *parts.get(start + 1)?);
    let prefix = if start == 1 { "/mobile" } else { "" };
    url.path()
        .starts_with(&format!("{prefix}/{user}/{presentation}"))
        .then(|| (user.to_owned(), presentation.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((user, presentation)) = target(&url) else {
        return crate::sites::fail(error("unsupported SlideShare URL"));
    };
    crate::sites::once(async move {
        let url = http.api(&format!("https://www.slideshare.net/{user}/{presentation}"))?;
        let page = http.text(&url).await?;
        let items = parse_page(&page, &presentation)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &PRESENTATION,
            items,
        )))
    })
}

fn next_data(page: &str) -> Result<Value> {
    let data = Some(extr(
        page,
        " id=\"__NEXT_DATA__\" type=\"application/json\">",
        "</script>",
    ))
    .filter(|data| !data.is_empty())
    .unwrap_or_else(|| {
        extr(
            page,
            " id='__NEXT_DATA__' type='application/json'>",
            "</script>",
        )
    });
    super::json_loads(data, "SlideShare")
}

fn trimmed(slideshow: &Value, key: &str) -> Value {
    match slideshow.get(key) {
        Some(Value::String(text)) => text.trim().into(),
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

pub(crate) fn parse_page(page: &str, presentation: &str) -> Result<Vec<Item>> {
    let data = next_data(page)?;
    let slideshow = data
        .pointer("/props/pageProps/slideshow")
        .ok_or_else(|| error("SlideShare page contains no slideshow"))?;
    let created: String = str_of(slideshow, "createdAt").chars().take(19).collect();
    let created = super::parse_iso(&created);
    let mut meta = Map::new();
    meta.insert(
        "user".into(),
        slideshow.get("username").cloned().unwrap_or(Value::Null),
    );
    meta.insert("presentation".into(), presentation.into());
    meta.insert("title".into(), trimmed(slideshow, "title"));
    meta.insert("description".into(), trimmed(slideshow, "description"));
    meta.insert(
        "views".into(),
        slideshow.get("views").cloned().unwrap_or(Value::Null),
    );
    meta.insert(
        "likes".into(),
        slideshow.get("likes").cloned().unwrap_or(Value::Null),
    );
    meta.insert("date".into(), super::date(created));

    let slides = slideshow.get("slides").unwrap_or(&Value::Null);
    let begin = format!(
        "{}/{}/95/{}-",
        str_of(slides, "host"),
        str_of(slides, "imageLocation"),
        str_of(slides, "title")
    );
    let total = slideshow
        .get("totalSlides")
        .and_then(Value::as_u64)
        .unwrap_or_default()
        .min(10_000);
    let id = slideshow
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_else(|| presentation.to_owned());
    let user = meta.get("user").and_then(Value::as_str).map(str::to_owned);
    let title = meta.get("title").and_then(Value::as_str).map(str::to_owned);
    let mut items = Vec::new();
    for num in 1..=total {
        let Some(url) = super::file_url(
            &format!("{begin}{num}-1024.jpg"),
            "https://www.slideshare.net/",
        ) else {
            continue;
        };
        let mut fields = meta.clone();
        fields.insert("count".into(), total.into());
        fields.insert("num".into(), num.into());
        super::nameext_into(url.as_str(), &mut fields);
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), num.to_string())
                .user(user.clone())
                .caption(title.clone())
                .timestamp(created),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.slideshare.net/Slideshare/get-started-with-slide-share"),
            Some(("Slideshare".into(), "get-started-with-slide-share".into()))
        );
        assert_eq!(
            t("https://slideshare.net/mobile/u/p"),
            Some(("u".into(), "p".into()))
        );
        assert_eq!(
            t("https://slideshare.net/mobile/p"),
            Some(("mobile".into(), "p".into()))
        );
        for raw in [
            "https://www.slideshare.net/u",
            "https://www.slideshare.net.evil.com/u/p",
            "https://evilslideshare.net/u/p",
            "https://de.slideshare.net/u/p",
            "https://www.slideshare.net:8080/u/p",
            "https://u:p@www.slideshare.net/u/p",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_slides() {
        let page = r#"<script id="__NEXT_DATA__" type="application/json">{"props": {"pageProps": {"slideshow": {
            "id": 42, "username": "Slideshare", "title": " Get Started ", "description": " d ",
            "views": 10, "likes": 2, "createdAt": "2012-03-04 05:06:07 UTC", "totalSlides": 2,
            "slides": {"host": "https://image.slidesharecdn.com", "imageLocation": "loc",
            "title": "get-started"}}}}}</script>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("slideshare", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.slideshare.net/mobile/Slideshare/get-started").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://image.slidesharecdn.com/loc/95/get-started-2-1024.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Get Started");
        assert_eq!(k["description"], "d");
        assert_eq!(k["date"], "2012-03-04 05:06:07");
        assert_eq!(k["count"], 2);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "get-started-02.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("slideshare/Slideshare")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{presentation}_{num}",["get-started","1"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /Slideshare/get-started "));
    }
}
