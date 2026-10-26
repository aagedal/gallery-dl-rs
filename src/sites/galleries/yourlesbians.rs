//! YourLesbians albums, mirroring gallery_dl/extractor/yourlesbians.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extract_iter, group, re, rpartition, split_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static ALBUM: Route = Route::new(
    "album",
    "{num:>03} {filename}.{extension}",
    &["{category}", "{title}"],
    "{title}/{num}",
);

pub static SITE: Site = Site {
    category: "yourlesbians",
    name: "YourLesbians",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<(String, String)> {
    captures(
        re!(r"^https?://(?:www\.)?yourlesbians\.com(/album/([^/?#]+)/?)"),
        url,
    )
    .map(|c| (group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported YourLesbians URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text(&http.api(&format!("https://yourlesbians.com{path}"))?)
            .await?;
        let mut scan = Scan::new(&page);
        let mut data = Map::new();
        data.insert(
            "album_url".into(),
            scan.extr("property=\"og:url\" content=\"", "\"").into(),
        );
        let title: String = scan
            .extr("property=\"og:title\" content=\"", "\"")
            .to_owned();
        let title = title
            .char_indices()
            .rev()
            .nth(7)
            .map_or("", |(i, _)| &title[..i]);
        data.insert("title".into(), unescape(title.trim_end()).into());
        data.insert(
            "album_thumbnail".into(),
            scan.extr("property=\"og:image\" content=\"", "\"").into(),
        );
        let description = scan.extr("property=\"og:description\" content=\"", "\"");
        let description = if description.ends_with(", right after.") {
            ""
        } else {
            description
        };
        data.insert("description".into(), description.into());
        let tags: Vec<Value> = split_html(scan.extr("tags-row", "</div>"))
            .into_iter()
            .skip(1)
            .map(Value::from)
            .collect();
        data.insert("tags".into(), tags.into());
        let album = scan.extr("class=\"album-inner", "</div>");
        let images = extract_iter(album, "<a href=\"", "\"")
            .into_iter()
            .map(|url| {
                let name = url.rsplit('/').nth(1).unwrap_or_default();
                let (stem, _, ext) = rpartition(name, ".");
                let mut file = Map::new();
                file.insert("filename".into(), stem.into());
                file.insert("extension".into(), ext.into());
                (url.to_owned(), Some(file))
            })
            .collect();
        Ok(crate::sites::events(super::numbered(
            &SITE, http, &ALBUM, &data, images, &slug,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[tokio::test]
    async fn names_files_from_parent_segment() {
        let page = r#"<meta property="og:url" content="https://yourlesbians.com/album/x/"><meta property="og:title" content="Two Girls - Album">
            <meta property="og:image" content="t.jpg"><meta property="og:description" content="Watch, right after.">
            <div class="tags-row"><a>kiss</a></div><div class="album-inner"><a href="https://cdn/get_image/2/abc/photo1.jpg/"></a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("yourlesbians", root);
        let files = media(extract(
            &http,
            Url::parse("https://yourlesbians.com/album/x/").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Two Girls");
        assert_eq!(k["description"], "");
        assert_eq!(k["tags"], serde_json::json!(["kiss"]));
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001 photo1.jpg");
        server.join().unwrap();
    }
}
