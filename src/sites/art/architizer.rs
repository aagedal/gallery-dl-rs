//! Architizer projects and firms, mirroring
//! gallery_dl/extractor/architizer.py: a project page names its images in
//! `og:image:secure_url` tags; a firm page links its projects.
use super::{Scan, extract_iter, nameext_into, segment};
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
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

/// `architizer\.com/(projects|firms)/([^/?#]+)`, without `www.`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("architizer.com") {
        return None;
    }
    let path = url.path();
    let (kind, rest) = if let Some(rest) = path.strip_prefix("/projects/") {
        (true, rest)
    } else {
        (false, path.strip_prefix("/firms/")?)
    };
    let name = rest.split('/').next().filter(|name| segment(name))?;
    Some(if kind {
        Target::Project(name.into())
    } else {
        Target::Firm(name.into())
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Project(name)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/projects/{name}/"))?)
                .await?;
            let items = parse_project(&page)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &PROJECT, items,
            )))
        }),
        Some(Target::Firm(name)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/firms/{name}/?requesting_merlin=pages"))?)
                .await?;
            let children: Vec<Url> = extract_iter(&page, "<a href=\"/projects/", "\"")
                .into_iter()
                .filter(|project| !project.starts_with("q/"))
                .filter_map(|project| Url::parse(&format!("{ROOT}/projects/{project}")).ok())
                .filter(|url| target(url).is_some())
                .collect();
            Ok(stream::iter(children)
                .map(move |url| crate::sites::queue(http, "architizer", "firm", url, &Map::new()))
                .flatten()
                .boxed())
        }),
        None => crate::sites::fail(error("unsupported Architizer URL")),
    }
}

/// GalleryExtractor `metadata()` and `images()` with upstream numbering.
pub(crate) fn parse_project(page: &str) -> Result<Vec<Item>> {
    let mut scan = Scan::new(page);
    scan.extr("id=\"Pages\"", "");
    let mut data = Map::new();
    data.insert("title".into(), scan.extr("data-name='", "'").into());
    data.insert("slug".into(), scan.extr("data-slug='", "'").into());
    let gid = scan.extr("data-gid='", "'");
    let gid = gid.rsplit_once('.').map_or(gid, |(_, id)| id);
    data.insert("gid".into(), gid.into());
    data.insert(
        "firm".into(),
        scan.extr("data-firm-leaders-str='", "'").into(),
    );
    data.insert("location".into(), scan.extr("<h2>", "<").trim().into());
    let kind = scan.extr("<div class=\"title\">Type</div>", "<br");
    data.insert("type".into(), unescape(&remove_html(kind)).into());
    for (key, label) in [("status", "STATUS"), ("year", "YEAR"), ("size", "SIZE")] {
        let value = scan.extr(&format!("<div class=\"title\">{label}</div>"), "</");
        data.insert(key.into(), remove_html(value).into());
    }
    let description = scan.extr("<span class=\"copy js-copy\">", "</span></div>");
    data.insert(
        "description".into(),
        unescape(&description.replace("<br />", "\n")).into(),
    );
    let urls: Vec<(String, Url)> =
        extract_iter(page, "property=\"og:image:secure_url\" content=\"", "?")
            .into_iter()
            .filter_map(|raw| Some((raw.to_owned(), super::file_url(raw, ROOT)?)))
            .collect();
    if urls.is_empty() && gid.is_empty() {
        return Err(error("Architizer page has no project"));
    }
    let count = urls.len();
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    let firm = data.get("firm").and_then(Value::as_str).map(str::to_owned);
    Ok(urls
        .into_iter()
        .enumerate()
        .map(|(index, (raw, url))| {
            let mut fields = data.clone();
            fields.insert("count".into(), count.into());
            fields.insert("num".into(), (index + 1).into());
            nameext_into(&raw, &mut fields);
            Item::new(url, fields)
                .ids(gid, (index + 1).to_string())
                .user(firm.clone())
                .caption(title.clone())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://architizer.com/projects/house/"),
            Some(Target::Project("house".into()))
        );
        assert_eq!(
            t("http://architizer.com/firms/studio"),
            Some(Target::Firm("studio".into()))
        );
        for raw in [
            "https://www.architizer.com/projects/house/",
            "https://architizer.com/projects/",
            "https://architizer.com.evil/projects/house",
            "https://architizer.com:81/projects/house",
            "https://u:p@architizer.com/projects/house",
            "https://architizer.com/blog/house",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_project() {
        let page = r#"<div id="Pages" data-name='House A' data-slug='house-a'
            data-gid='project.123' data-firm-leaders-str='Studio X'>
            <h2> Oslo </h2>
            <div class="title">Type</div> <b>Residential &amp; more</b><br>
            <div class="title">STATUS</div> <i>Built</i></div>
            <div class="title">YEAR</div>2020</div>
            <div class="title">SIZE</div>100 sqm</div>
            <span class="copy js-copy">Line<br />Two &amp; three</span></div>
            <meta property="og:image:secure_url" content="https://images.architizer.com/a/1.jpg?w=1">
            <meta property="og:image:secure_url" content="https://images.architizer.com/a/2.png?w=1">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("architizer", root);
        let files = media(extract(
            &http,
            Url::parse("https://architizer.com/projects/house-a/photos").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["gid"], "123");
        assert_eq!(k["type"], "Residential & more");
        assert_eq!(k["status"], "Built");
        assert_eq!(k["year"], "2020");
        assert_eq!(k["location"], "Oslo");
        assert_eq!(k["description"], "Line\nTwo & three");
        assert_eq!(k["count"], 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://images.architizer.com/a/2.png"
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("architizer/Studio X/House A")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{gid}_{num}",["123","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /projects/house-a/ "));
    }

    #[tokio::test]
    async fn queues_firm_projects() {
        let page = r#"<a href="/projects/one/">1</a><a href="/projects/q/x">q</a>
            <a href="/projects/two">2</a>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("architizer", root);
        let children = queued(extract(
            &http,
            Url::parse("https://architizer.com/firms/studio/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /firms/studio/?requesting_merlin=pages "));
    }
}
