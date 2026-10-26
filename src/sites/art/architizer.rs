//! Architizer projects and firms, mirroring gallery_dl/extractor/architizer.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extract_iter, group, re, remove_html, rpartition, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://architizer.com";

static PROJECT: Route = Route::new(
    "project",
    "{filename}.{extension}",
    &["{category}", "{firm}", "{title}"],
    "{gid}_{num}",
);
static FIRM: Route = Route::new("firm", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "architizer",
    name: "Architizer",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&PROJECT, &FIRM],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Project(String),
    Firm(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://architizer\.com/projects/([^/?#]+)"), url) {
        return Some(Target::Project(group(&c, 1).into()));
    }
    captures(re!(r"^https?://architizer\.com/firms/([^/?#]+)"), url)
        .map(|c| Target::Firm(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Architizer URL"));
    };
    crate::sites::once(async move {
        match target {
            Target::Project(slug) => {
                let page = http
                    .text(&http.api(&format!("{ROOT}/projects/{slug}/"))?)
                    .await?;
                let mut scan = Scan::new(&page);
                scan.extr("id=\"Pages\"", "");
                let mut data = Map::new();
                data.insert("title".into(), scan.extr("data-name='", "'").into());
                data.insert("slug".into(), scan.extr("data-slug='", "'").into());
                data.insert(
                    "gid".into(),
                    rpartition(scan.extr("data-gid='", "'"), ".").2.into(),
                );
                data.insert(
                    "firm".into(),
                    scan.extr("data-firm-leaders-str='", "'").into(),
                );
                data.insert("location".into(), scan.extr("<h2>", "<").trim().into());
                data.insert(
                    "type".into(),
                    unescape(&remove_html(
                        scan.extr("<div class=\"title\">Type</div>", "<br"),
                    ))
                    .into(),
                );
                data.insert(
                    "status".into(),
                    remove_html(scan.extr("<div class=\"title\">STATUS</div>", "</")).into(),
                );
                data.insert(
                    "year".into(),
                    remove_html(scan.extr("<div class=\"title\">YEAR</div>", "</")).into(),
                );
                data.insert(
                    "size".into(),
                    remove_html(scan.extr("<div class=\"title\">SIZE</div>", "</")).into(),
                );
                data.insert(
                    "description".into(),
                    unescape(
                        &scan
                            .extr("<span class=\"copy js-copy\">", "</span></div>")
                            .replace("<br />", "\n"),
                    )
                    .into(),
                );
                let gid = crate::sites::scalar(&data["gid"]);
                let images =
                    extract_iter(&page, "property=\"og:image:secure_url\" content=\"", "?")
                        .into_iter()
                        .map(|u| (u.to_owned(), None))
                        .collect();
                Ok(crate::sites::events(crate::sites::galleries::numbered(
                    &SITE, http, &PROJECT, &data, images, &gid,
                )))
            }
            Target::Firm(firm) => {
                let page = http
                    .text(&http.api(&format!("{ROOT}/firms/{firm}/?requesting_merlin=pages"))?)
                    .await?;
                let children = extract_iter(&page, "<a href=\"/projects/", "\"")
                    .into_iter()
                    .filter(|project| !project.starts_with("q/"))
                    .filter_map(|project| {
                        Some((
                            Url::parse(&format!("{ROOT}/projects/{project}")).ok()?,
                            Map::new(),
                        ))
                    })
                    .collect();
                Ok(crate::sites::galleries::queue(http, &SITE, &FIRM, children))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[tokio::test]
    async fn extracts_project() {
        let page = r#"<div data-name='House' id="Pages" data-name='Tower' data-slug='tower' data-gid='a.b.42' data-firm-leaders-str='Firm'><h2> Oslo </h2>
            <div class="title">Type</div>Commercial &amp; Office<br><div class="title">STATUS</div><span>Built</span></div>
            <meta property="og:image:secure_url" content="https://img/1.jpg?w=1"><meta property="og:image:secure_url" content="https://img/2.jpg?w=1">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("architizer", root);
        let files = media(extract(
            &http,
            Url::parse("https://architizer.com/projects/tower/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["gid"].as_str(),
                k["type"].as_str(),
                k["status"].as_str()
            ),
            (
                Some("Tower"),
                Some("42"),
                Some("Commercial & Office"),
                Some("Built")
            )
        );
        let (_, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("architizer/Firm/Tower")
        );
        server.join().unwrap();
    }
}
