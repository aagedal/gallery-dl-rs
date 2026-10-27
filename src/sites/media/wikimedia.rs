//! MediaWiki sites, mirroring gallery_dl/extractor/wikimedia.py through the
//! `action=query` API with `prop=imageinfo`.
//!
//! The `wikimedia` instance derives its category from the host
//! (`en.wikipedia.org` is `wikipedia`), so each project has its own site.
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{filename} ({sha1[:8]}).{extension}";
const ARCHIVE: &str = "{sha1}";
const ARTICLE_DIR: &[&str] = &["{category}", "{page}"];
static ARTICLE: Route = Route::new("article", FILENAME, ARTICLE_DIR, ARCHIVE);
static WIKI: Route = Route::new("wiki", FILENAME, &["{category}"], ARCHIVE);
// Upstream sets an article's subcategory to its lowercased title prefix;
// common MediaWiki namespaces get their own route, others stay `article`.
static CATEGORY: Route = Route::new("category", FILENAME, ARTICLE_DIR, ARCHIVE);
static FILE: Route = Route::new("file", FILENAME, ARTICLE_DIR, ARCHIVE);
static IMAGE: Route = Route::new("image", FILENAME, ARTICLE_DIR, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, ARTICLE_DIR, ARCHIVE);
static TEMPLATE: Route = Route::new("template", FILENAME, ARTICLE_DIR, ARCHIVE);
static HELP: Route = Route::new("help", FILENAME, ARTICLE_DIR, ARCHIVE);
static PORTAL: Route = Route::new("portal", FILENAME, ARTICLE_DIR, ARCHIVE);
static SPECIAL: Route = Route::new("special", FILENAME, ARTICLE_DIR, ARCHIVE);
static TALK: Route = Route::new("talk", FILENAME, ARTICLE_DIR, ARCHIVE);
static ROUTES: &[&Route] = &[
    &ARTICLE, &WIKI, &CATEGORY, &FILE, &IMAGE, &USER, &TEMPLATE, &HELP, &PORTAL, &SPECIAL, &TALK,
];

fn prefix_route(prefix: Option<&str>) -> &'static Route {
    match prefix {
        Some("category") => &CATEGORY,
        Some("file") => &FILE,
        Some("image") => &IMAGE,
        Some("user") => &USER,
        Some("template") => &TEMPLATE,
        Some("help") => &HELP,
        Some("portal") => &PORTAL,
        Some("special") => &SPECIAL,
        Some("talk") => &TALK,
        _ => &ARTICLE,
    }
}

/// How an instance's hosts and root are defined.
#[derive(Clone, Copy)]
enum Hosts {
    /// `[a-z]{2,}.<project>.org`; the root is the matched host.
    Project(&'static str),
    /// Fixed root; any of these hosts matches.
    Fixed {
        root: &'static str,
        hosts: &'static [&'static str],
    },
    /// `<wiki>.<domain>` farms (`fandom`, `wikigg`): the root is the matched
    /// host, the category `<site>-<wiki>`, and files default to originals.
    Farm { domain: &'static str, dash: bool },
}

struct Instance {
    hosts: Hosts,
    api_path: &'static str,
    user_agent: Option<&'static str>,
}

macro_rules! wiki {
    ($ident:ident, $instance:ident, $category:literal, $name:literal, $def:expr) => {
        static $instance: Instance = $def;
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$instance, url).is_some(),
            extract: |http, url| extract(&$ident, &$instance, http, url),
            routes: ROUTES,
            api_key: false,
            cookies: None,
        };
    };
}

macro_rules! project {
    ($ident:ident, $instance:ident, $category:literal, $name:literal) => {
        wiki!(
            $ident,
            $instance,
            $category,
            $name,
            Instance {
                hosts: Hosts::Project($category),
                api_path: "/w/api.php",
                user_agent: None,
            }
        );
    };
}

macro_rules! fixed {
    ($ident:ident, $instance:ident, $category:literal, $name:literal, $root:literal, $hosts:expr, $api:literal, $ua:expr) => {
        wiki!(
            $ident,
            $instance,
            $category,
            $name,
            Instance {
                hosts: Hosts::Fixed {
                    root: $root,
                    hosts: $hosts,
                },
                api_path: $api,
                user_agent: $ua,
            }
        );
    };
}

project!(WIKIPEDIA, WIKIPEDIA_INSTANCE, "wikipedia", "Wikipedia");
project!(WIKTIONARY, WIKTIONARY_INSTANCE, "wiktionary", "Wiktionary");
project!(WIKIQUOTE, WIKIQUOTE_INSTANCE, "wikiquote", "Wikiquote");
project!(WIKIBOOKS, WIKIBOOKS_INSTANCE, "wikibooks", "Wikibooks");
project!(WIKISOURCE, WIKISOURCE_INSTANCE, "wikisource", "Wikisource");
project!(WIKINEWS, WIKINEWS_INSTANCE, "wikinews", "Wikinews");
project!(
    WIKIVERSITY,
    WIKIVERSITY_INSTANCE,
    "wikiversity",
    "Wikiversity"
);
project!(WIKIDATA, WIKIDATA_INSTANCE, "wikidata", "Wikidata");
project!(WIKIVOYAGE, WIKIVOYAGE_INSTANCE, "wikivoyage", "Wikivoyage");
fixed!(
    WIKISPECIES,
    WIKISPECIES_INSTANCE,
    "wikispecies",
    "Wikispecies",
    "https://species.wikimedia.org",
    &["species.wikimedia.org"],
    "/w/api.php",
    None
);
fixed!(
    WIKIMEDIACOMMONS,
    WIKIMEDIACOMMONS_INSTANCE,
    "wikimediacommons",
    "Wikimedia Commons",
    "https://commons.wikimedia.org",
    &["commons.wikimedia.org"],
    "/w/api.php",
    None
);
fixed!(
    MEDIAWIKI,
    MEDIAWIKI_INSTANCE,
    "mediawiki",
    "MediaWiki",
    "https://www.mediawiki.org",
    &["mediawiki.org", "www.mediawiki.org"],
    "/w/api.php",
    None
);
macro_rules! farm {
    ($ident:ident, $instance:ident, $category:literal, $name:literal, $domain:literal, $dash:literal) => {
        wiki!(
            $ident,
            $instance,
            $category,
            $name,
            Instance {
                hosts: Hosts::Farm {
                    domain: $domain,
                    dash: $dash,
                },
                api_path: "/api.php",
                user_agent: None,
            }
        );
    };
}

farm!(
    FANDOM,
    FANDOM_INSTANCE,
    "fandom",
    "Fandom",
    "fandom.com",
    true
);
farm!(
    WIKIGG,
    WIKIGG_INSTANCE,
    "wikigg",
    "wiki.gg",
    "wiki.gg",
    false
);
fixed!(
    MARIOWIKI,
    MARIOWIKI_INSTANCE,
    "mariowiki",
    "Super Mario Wiki",
    "https://www.mariowiki.com",
    &["mariowiki.com", "www.mariowiki.com"],
    "/api.php",
    None
);
fixed!(
    BULBAPEDIA,
    BULBAPEDIA_INSTANCE,
    "bulbapedia",
    "Bulbapedia",
    "https://bulbapedia.bulbagarden.net",
    &["bulbapedia.bulbagarden.net", "archives.bulbagarden.net"],
    "/w/api.php",
    None
);
fixed!(
    PIDGIWIKI,
    PIDGIWIKI_INSTANCE,
    "pidgiwiki",
    "PidgiWiki",
    "https://www.pidgi.net",
    &["pidgi.net", "www.pidgi.net"],
    "/wiki/api.php",
    None
);
fixed!(
    AZURLANEWIKI,
    AZURLANEWIKI_INSTANCE,
    "azurlanewiki",
    "Azur Lane Wiki",
    "https://azurlane.koumakan.jp",
    &["azurlane.koumakan.jp"],
    "/w/api.php",
    Some("Googlebot-Image/1.0")
);
fixed!(
    MGEWIKI,
    MGEWIKI_INSTANCE,
    "mgewiki",
    "MGE Wiki",
    "https://mgewiki.moe",
    &["mgewiki.moe", "www.mgewiki.moe"],
    "/api.php",
    None
);

#[derive(Debug, PartialEq)]
struct Target {
    /// Instance root, including a `/xx` language path.
    root: String,
    lang: String,
    /// `<site>-<wiki>` for farm instances.
    category: Option<String>,
    kind: Kind,
}

#[derive(Debug, PartialEq)]
enum Kind {
    Wiki,
    Article {
        path: String,
        prefix: Option<String>,
    },
}

/// The registry category of a farm instance.
fn sites_category(instance: &Instance) -> &'static str {
    if std::ptr::eq(instance, &FANDOM_INSTANCE) {
        "fandom"
    } else {
        "wikigg"
    }
}

fn target(instance: &Instance, url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let (mut root, mut lang) = match instance.hosts {
        Hosts::Project(project) => {
            let label = host.strip_suffix(&format!(".{project}.org"))?;
            if label.len() < 2 || !label.bytes().all(|b| b.is_ascii_lowercase()) {
                return None;
            }
            // `labels[-3][-2:]` of the root URL.
            (
                format!("{}://{host}", url.scheme()),
                label[label.len() - 2..].to_owned(),
            )
        }
        Hosts::Fixed { root, hosts } => {
            if !hosts.contains(&host) {
                return None;
            }
            (root.to_owned(), String::new())
        }
        Hosts::Farm { domain, dash } => {
            let label = host.strip_suffix(&format!(".{domain}"))?;
            let valid = |c: char| c.is_alphanumeric() || c == '_' || (dash && c == '-');
            if label.is_empty() || !label.chars().all(valid) {
                return None;
            }
            (format!("{}://{host}", url.scheme()), "en".to_owned())
        }
    };
    let category = match instance.hosts {
        Hosts::Farm { .. } => host.split('.').next().map(|wiki| {
            let site = sites_category(instance);
            format!("{site}-{wiki}")
        }),
        _ => None,
    };
    let raw = &url.path()[1..];
    if raw.is_empty() {
        // `/?$`
        return (url.query().is_none() && url.fragment().is_none()).then_some(Target {
            root,
            lang,
            category,
            kind: Kind::Wiki,
        });
    }
    if raw.starts_with("static/") {
        return None;
    }
    let mut path = raw;
    if path.as_bytes().get(2) == Some(&b'/') && path.is_char_boundary(2) {
        lang = path[..2].to_owned();
        root = format!("{root}/{lang}");
        path = &path[3..];
    }
    if let Some(rest) = path.strip_prefix("wiki/") {
        path = rest;
    } else if let Some(rest) = path.strip_prefix("index.php/") {
        path = rest;
    }
    let prefix = path.split_once(':').map(|(pre, _)| pre.to_lowercase());
    Some(Target {
        root,
        lang,
        category,
        kind: Kind::Article {
            path: crate::sites::percent_decode(path),
            prefix,
        },
    })
}

fn extract<'a>(
    site: &'static Site,
    instance: &'static Instance,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some(target) = target(instance, &url) else {
        return crate::sites::fail(error("unsupported MediaWiki URL"));
    };
    let per_page = http
        .option("limit")
        .map_or_else(|| "50".into(), crate::sites::scalar);
    let (route, params, info, subcategories) = match &target.kind {
        Kind::Wiki => (
            &WIKI,
            vec![
                ("generator", "allpages".to_owned()),
                ("gapnamespace", "6".to_owned()),
                ("gaplimit", per_page),
            ],
            Map::new(),
            false,
        ),
        Kind::Article { path, prefix } => {
            let mut info = Map::new();
            info.insert("page".into(), path.clone().into());
            info.insert("lang".into(), target.lang.clone().into());
            let (params, subcategories) = match prefix.as_deref() {
                Some("category") => (
                    vec![
                        ("generator", "categorymembers".to_owned()),
                        ("gcmtitle", path.clone()),
                        ("gcmtype", "file".to_owned()),
                        ("gcmlimit", per_page),
                    ],
                    http.option_bool("subcategories", true),
                ),
                Some("file") => (vec![("titles", path.clone())], false),
                _ => (
                    vec![
                        ("generator", "images".to_owned()),
                        ("gimlimit", per_page),
                        ("titles", path.clone()),
                    ],
                    false,
                ),
            };
            (prefix_route(prefix.as_deref()), params, info, subcategories)
        }
    };
    let mut info = info;
    if let Some(category) = &target.category {
        info.insert("_category".into(), category.as_str().into());
    }
    let mut params: Vec<(String, String)> =
        params.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    let revisions = match http.option("image-revisions") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => "1".into(),
    };
    for (key, value) in [
        ("action", "query"),
        ("format", "json"),
        ("prop", "imageinfo"),
        (
            "iiprop",
            "timestamp|user|userid|comment|canonicaltitle|url|size|sha1|mime|metadata|commonmetadata|extmetadata|bitdepth",
        ),
        ("iilimit", revisions.as_str()),
    ] {
        params.push((key.into(), value.into()));
    }
    let api = format!("{}{}", target.root, instance.api_path);
    let files = query(
        site,
        instance,
        http,
        route,
        api.clone(),
        params.clone(),
        info,
    );
    if !subcategories {
        return files;
    }
    // Subcategories are queued as category articles after the files.
    let base = format!("{}/wiki/", target.root);
    for (key, value) in &mut params {
        if key == "gcmtype" {
            *value = "subcat".into();
        }
    }
    let subcats =
        pages(instance, http, api, params, move |pages| {
            let children: Vec<_> = pages
                .iter()
                .filter_map(|page| {
                    let title = crate::sites::media::str_of(page, "title").replace(' ', "_");
                    let url = Url::parse(&format!("{base}{title}")).ok()?;
                    Some((url, page.as_object()?.clone()))
                })
                .collect();
            futures_util::stream::iter(children.into_iter().map(|(url, data)| {
                crate::sites::queue(http, site.category, "category", url, &data)
            }))
            .flatten()
            .boxed()
        });
    files.chain(subcats).boxed()
}

fn query<'a>(
    site: &'static Site,
    instance: &'static Instance,
    http: &'a Http,
    route: &'static Route,
    api: String,
    params: Vec<(String, String)>,
    info: Map<String, Value>,
) -> Events<'a> {
    // Farm wikis default to original files (upstream `self.format`).
    let format = http
        .option_str("format")
        .map(str::to_owned)
        .or_else(|| matches!(instance.hosts, Hosts::Farm { .. }).then(|| "original".to_owned()));
    pages(instance, http, api, params, move |pages| {
        crate::sites::events(
            pages
                .into_iter()
                .flat_map(|page| page_events(site, http, route, page, &info, format.as_deref()))
                .collect(),
        )
    })
}

/// `_pagination`: follow `continue` until absent; an API `error` ends the
/// listing like upstream's logged error.
fn pages<'a, F>(
    instance: &'static Instance,
    http: &'a Http,
    api: String,
    params: Vec<(String, String)>,
    render: F,
) -> Events<'a>
where
    F: Fn(Vec<Value>) -> Events<'a> + Clone + Send + Sync + 'a,
{
    crate::sites::paginate(Some((params, 0u32)), move |state| {
        let api = api.clone();
        let render = render.clone();
        async move {
            let Some((params, page)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&api)?;
            url.query_pairs_mut().extend_pairs(&params);
            let headers: Vec<(&str, &str)> = instance
                .user_agent
                .map(|ua| vec![("User-Agent", ua)])
                .unwrap_or_default();
            let data = http.json_with(&url, &headers).await?;
            if let Some(failure) = data.get("error") {
                return Err(error(format!(
                    "MediaWiki API error {}: {}",
                    crate::sites::media::str_of(failure, "code"),
                    crate::sites::media::str_of(failure, "info")
                )));
            }
            let entries: Vec<Value> = data
                .get("query")
                .and_then(|q| q.get("pages"))
                .map(|pages| match pages {
                    Value::Object(map) => map.values().cloned().collect(),
                    Value::Array(list) => list.clone(),
                    _ => Vec::new(),
                })
                .unwrap_or_default();
            let next = data.get("continue").and_then(Value::as_object).map(|cont| {
                let mut params = params;
                for (key, value) in cont {
                    let value = crate::sites::scalar(value);
                    match params.iter_mut().find(|(k, _)| k == key) {
                        Some((_, v)) => *v = value,
                        None => params.push((key.clone(), value)),
                    }
                }
                params
            });
            let next = next.filter(|_| page < 100_000).map(|p| (p, page + 1));
            Ok(Some((render(entries), next)))
        }
    })
}

/// gallery-dl `text.nameext_from_name`.
fn nameext_from_name(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 16 => {
            (stem.to_owned(), ext.to_lowercase())
        }
        _ => (name.to_owned(), String::new()),
    }
}

fn page_events(
    site: &'static Site,
    http: &Http,
    route: &'static Route,
    page: Value,
    extra: &Map<String, Value>,
    format: Option<&str>,
) -> Vec<crate::event::ExtractionEvent> {
    let Some(mut info) = page.as_object().cloned() else {
        return Vec::new();
    };
    let images = match info.remove("imageinfo") {
        Some(Value::Array(images)) => images,
        _ => Vec::new(),
    };
    info.insert("count".into(), images.len().into());
    info.extend(extra.clone());
    let page_id = info
        .get("pageid")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let mut items = Vec::new();
    let mut num = 0;
    for image in images {
        let Some(mut image) = image.as_object().cloned() else {
            continue;
        };
        if image.contains_key("filemissing") {
            continue;
        }
        num += 1;
        image.insert("num".into(), num.into());
        for key in ["metadata", "commonmetadata"] {
            let entries = match image.get(key) {
                Some(Value::Array(list)) => list.clone(),
                _ => Vec::new(),
            };
            let mut map = Map::new();
            for entry in entries {
                if let Some(name) = entry.get("name").and_then(Value::as_str) {
                    map.insert(
                        name.to_owned(),
                        entry.get("value").cloned().unwrap_or(Value::Null),
                    );
                }
            }
            image.insert(key.into(), map.into());
        }
        let title = crate::sites::media::str_of(&Value::Object(image.clone()), "canonicaltitle")
            .split_once(':')
            .map(|(_, name)| name.to_owned())
            .unwrap_or_default();
        crate::sites::media::set_nameext(&mut image, nameext_from_name(&title));
        let ts = crate::sites::media::iso_ts(
            image.get("timestamp").and_then(Value::as_str).unwrap_or(""),
        );
        image.insert("date".into(), crate::sites::media::date(ts));
        let mut raw = image
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(format) = format.filter(|f| !f.is_empty()) {
            let sep = if raw.contains('?') { '&' } else { '?' };
            raw = format!("{raw}{sep}format={format}");
            image.insert("url".into(), raw.clone().into());
        }
        let Some(url) = crate::sites::media::web_url(&raw) else {
            continue;
        };
        // `image.update(info)`: page fields win.
        image.extend(info.clone());
        let sha1 = image
            .get("sha1")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let user = image.get("user").and_then(Value::as_str).map(str::to_owned);
        let caption = image
            .get("comment")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(
            Item::new(url, image)
                .ids(page_id.clone(), sha1)
                .user(user)
                .caption(caption)
                .timestamp(ts),
        );
    }
    crate::sites::post(site, http, route, items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn farm_wikis() {
        let t = |i: &Instance, raw: &str| target(i, &Url::parse(raw).unwrap());
        let fandom = t(
            &FANDOM_INSTANCE,
            "https://mushishi.fandom.com/wiki/Yahagi_no_Kiyo",
        )
        .unwrap();
        assert_eq!(
            (fandom.root.as_str(), fandom.category.as_deref()),
            ("https://mushishi.fandom.com", Some("fandom-mushishi"))
        );
        let gg = t(
            &WIKIGG_INSTANCE,
            "https://hearthstone.wiki.gg/wiki/Flame_Juggler",
        )
        .unwrap();
        assert_eq!(gg.category.as_deref(), Some("wikigg-hearthstone"));
        assert!(t(&WIKIGG_INSTANCE, "https://my-wiki.wiki.gg/").is_none());
        let url = Url::parse("https://mushishi.fandom.com/wiki/X").unwrap();
        assert_eq!(crate::sites::find(&url).map(|s| s.category), Some("fandom"));
    }

    #[test]
    fn classifies_urls() {
        let wp = &WIKIPEDIA_INSTANCE;
        let t = |i: &Instance, raw: &str| target(i, &Url::parse(raw).unwrap());
        assert_eq!(
            t(wp, "https://en.wikipedia.org/wiki/Athena_Parthenos"),
            Some(Target {
                root: "https://en.wikipedia.org".into(),
                lang: "en".into(),
                category: None,
                kind: Kind::Article {
                    path: "Athena_Parthenos".into(),
                    prefix: None
                }
            })
        );
        assert_eq!(
            t(wp, "https://de.wikipedia.org/wiki/Kategorie:Bild%20X").map(|t| t.kind),
            Some(Kind::Article {
                path: "Kategorie:Bild X".into(),
                prefix: Some("kategorie".into())
            })
        );
        assert_eq!(
            t(wp, "https://en.wikipedia.org/").map(|t| t.kind),
            Some(Kind::Wiki)
        );
        let commons = &WIKIMEDIACOMMONS_INSTANCE;
        assert!(t(commons, "https://commons.wikimedia.org/wiki/File:A.jpg").is_some());
        for raw in [
            "https://en.m.wikipedia.org/wiki/X",
            "https://en.wikipedia.org.evil.test/wiki/X",
            "https://evil.test/en.wikipedia.org/wiki/X",
            "https://en.wikipedia.org:8443/wiki/X",
            "https://en.wikipedia.org/static/x.png",
            "https://EN1.wikipedia.org/wiki/X",
        ] {
            assert_eq!(t(wp, raw), None, "{raw}");
        }
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(
            find("https://fr.wiktionary.org/wiki/chat"),
            Some("wiktionary")
        );
        assert_eq!(find("https://www.wikidata.org/wiki/Q1"), Some("wikidata"));
        assert_eq!(
            find("https://archives.bulbagarden.net/wiki/File:X.png"),
            Some("bulbapedia")
        );
        assert_eq!(
            find("https://commons.wikimedia.org/wiki/File:A.jpg"),
            Some("wikimediacommons")
        );
    }

    fn page(id: u64, title: &str) -> Value {
        json!({"pageid": id, "ns": 6, "title": title, "imagecount": 3,
            "imageinfo": [{"timestamp": "2017-07-14T02:40:00Z", "user": "Uploader",
                "canonicaltitle": title, "sha1": "0123456789abcdef", "size": 10,
                "url": format!("https://upload.wikimedia.org/wikipedia/commons/a/ab/{}",
                    title.trim_start_matches("File:").replace(' ', "_")),
                "metadata": [{"name": "Width", "value": 10}], "commonmetadata": null}]})
    }

    #[tokio::test]
    async fn article_paginates_with_continue() {
        let (root, server) = serve(vec![
            json(
                json!({"continue": {"gimcontinue": "1|2", "continue": "gimcontinue||"},
                "query": {"pages": {"1": page(1, "File:One.JPG")}}}),
            ),
            json(json!({"query": {"pages": {"2": page(2, "File:Two words.png")}}})),
        ]);
        let http = http("wikipedia", root);
        let files = media(extract(
            &WIKIPEDIA,
            &WIKIPEDIA_INSTANCE,
            &http,
            Url::parse("https://en.wikipedia.org/wiki/Athena").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "One");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["page"], "Athena");
        assert_eq!(k["lang"], "en");
        assert_eq!(k["title"], "File:One.JPG");
        assert_eq!(k["count"], 1);
        assert_eq!(k["metadata"]["Width"], 10);
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "Two words (01234567).png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("wikipedia/Athena")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{sha1}",["0123456789abcdef"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /w/api.php?generator=images&gimlimit=50&titles=Athena&action=query&format=json&prop=imageinfo&iiprop="
            ),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("&iilimit=1 "));
        assert!(requests[1].contains("&iilimit=1&gimcontinue=1%7C2&continue=gimcontinue%7C%7C "));
    }

    #[tokio::test]
    async fn category_queues_subcategories() {
        let (root, server) = serve(vec![
            json(json!({"query": {"pages": {"1": page(1, "File:A.jpg")}}})),
            json(
                json!({"query": {"pages": {"5": {"pageid": 5, "ns": 14, "title": "Category:Sub cat"}}}}),
            ),
            json(json!({"query": {"pages": {"2": page(2, "File:B.jpg")}}})),
            json(json!({"query": {"pages": {}}})),
        ]);
        let http = http("wikimediacommons", root);
        let files = media(extract(
            &WIKIMEDIACOMMONS,
            &WIKIMEDIACOMMONS_INSTANCE,
            &http,
            Url::parse("https://commons.wikimedia.org/wiki/Category:Top").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "category");
        assert_eq!(keywords(&files[1])["page"], "Category:Sub_cat");
        let requests = server.join().unwrap();
        assert!(
            requests[0].contains("generator=categorymembers&gcmtitle=Category%3ATop&gcmtype=file")
        );
        assert!(requests[1].contains("gcmtype=subcat"));
        assert!(requests[2].contains("gcmtitle=Category%3ASub_cat&gcmtype=file"));
    }

    #[tokio::test]
    async fn wiki_lists_all_files() {
        let (root, server) = serve(vec![json(
            json!({"query": {"pages": {"1": page(1, "File:One.JPG")}}}),
        )]);
        let http = http("wikipedia", root);
        let files = media(extract(
            &WIKIPEDIA,
            &WIKIPEDIA_INSTANCE,
            &http,
            Url::parse("https://en.wikipedia.org/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "wiki");
        assert!(!keywords(&files[0]).contains_key("page"));
        assert_eq!(
            crate::sites::default_templates(&files[0])
                .unwrap()
                .1
                .render(&files[0])
                .unwrap()
                .unwrap(),
            std::path::PathBuf::from("wikipedia")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].contains("generator=allpages&gapnamespace=6&gaplimit=50&"));
    }

    #[tokio::test]
    async fn api_error_ends_with_failure() {
        let (root, server) = serve(vec![json(
            json!({"error": {"code": "badtitle", "info": "Bad title"}}),
        )]);
        let http = http("wikipedia", root);
        let results = extract(
            &WIKIPEDIA,
            &WIKIPEDIA_INSTANCE,
            &http,
            Url::parse("https://en.wikipedia.org/wiki/X").unwrap(),
        )
        .collect::<Vec<_>>()
        .await;
        let message = results[0].as_ref().unwrap_err().to_string();
        assert!(message.contains("badtitle: Bad title"), "{message}");
        server.join().unwrap();
    }
}
