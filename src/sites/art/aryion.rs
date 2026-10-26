//! Eka's Portal (aryion.com) galleries, favorites, watches, tags,
//! searches and posts, mirroring gallery_dl/extractor/aryion.py.
use crate::error;
use crate::sites::gdl::{
    Scan, auth_info, captures, date, extr, extract_iter, filename_from_contentdisposition, group,
    parse_int, parse_query, re, rextr, rpartition, split_html, strptime, truthy, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://aryion.com";
const BASE: &str = r"^https?://(?:www\.)?aryion\.com/g4";
const FILENAME: &str = "{id} {title}.{extension}";

static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &["{category}", "{user!l}", "{path:I}"],
    "{id}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{user!l}", "favorites", "{folder}"],
    "f_{user}_{id}",
);
static WATCH: Route = Route::new("watch", FILENAME, &["{category}", "{user!l}"], "{id}");
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "tags", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &[
        "{category}",
        "searches",
        "{search[prefix]}{search[q]|search[tags]|search[user]}",
    ],
    "s_{search[prefix]}{search[q]|search[tags]|search[user]}_{id}",
);
static POST: Route = Route::new(
    "post",
    FILENAME,
    &["{category}", "{user!l}", "{path:I}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "aryion",
    name: "Eka's Portal",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &FAVORITE, &WATCH, &TAG, &SEARCH, &POST],
    api_key: false,
    cookies: Some("aryion.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Favorite(String, String),
    Watch,
    Tag(String),
    Search(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(?:gallery/|user/|latest.php\?name=)([^/?#]+)"
        )),
        url,
    ) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/favorites/([^/?#]+)(?:/([^?#]+))?")),
        url,
    ) {
        return Some(Target::Favorite(group(&c, 1).into(), group(&c, 2).into()));
    }
    if captures(re!(&format!(r"{BASE}/messagepage\.php")), url).is_some() {
        return Some(Target::Watch);
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/tags\.php\?([^#]+)")), url) {
        return Some(Target::Tag(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/search\.php\?([^#]+)")), url) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/view/(\d+)")), url).map(|c| Target::Post(group(&c, 1).into()))
}

async fn login(http: &Http) -> crate::Result<()> {
    let base = http.api(ROOT)?;
    if http.cookie(&base, "phpbb3_rl7a3_sid").is_some() {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let url = http.api(&format!("{ROOT}/forum/ucp.php?mode=login"))?;
    let form = [
        ("username", username.as_str()),
        ("password", password.as_str()),
        ("login", "Login"),
    ];
    match http.post_form_or_location(&url, &form, &[]).await? {
        Ok(page) if page.contains("You have been successfully logged in.") => Ok(()),
        _ => Err(error(
            "Eka's Portal login failed; check the username and password",
        )),
    }
}

/// A parsed post: a file, a folder to descend into, or nothing.
enum Post {
    File(Box<Item>),
    Folder,
    Missing,
}

/// `_parse_post`: file headers from `data.php`, metadata from the page.
async fn parse_post(
    http: &Http,
    post_id: &str,
    user: Option<&str>,
    extra: &Map<String, Value>,
) -> crate::Result<Post> {
    let url = format!("{ROOT}/g4/data.php?id={post_id}");
    let (status, headers) = http.head(&http.api(&url)?, &[]).await?;
    if status >= 400 {
        return Ok(Post::Missing);
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    if matches!(
        header("content-type").as_str(),
        "application/x-folder"
            | "application/x-comic-folder"
            | "application/x-comic-folder-nomerge"
    ) {
        return Ok(Post::Folder);
    }
    let disposition = filename_from_contentdisposition(&header("content-disposition"));
    let (mut name, _, mut ext) = rpartition(&disposition, ".");
    if name.is_empty() {
        std::mem::swap(&mut name, &mut ext);
    }
    let mut modified = header("last-modified");
    if modified.len() > 24 && modified.as_bytes()[22] != b':' {
        modified = format!("{}:{} GMT", &modified[..22], &modified[22..24]);
    }
    let page = http
        .text(&http.api(&format!("{ROOT}/g4/view/{post_id}"))?)
        .await?;
    let mut scan = Scan::new(&page);
    let heading = unescape(scan.extr("<title>g4 :: ", "<"));
    let (title, _, artist) = rpartition(&heading, " by ");
    let mut data = Map::new();
    data.insert("id".into(), parse_int(post_id).into());
    data.insert("url".into(), url.as_str().into());
    data.insert(
        "user".into(),
        user.filter(|u| !u.is_empty()).unwrap_or(artist).into(),
    );
    data.insert("title".into(), title.into());
    data.insert("artist".into(), artist.into());
    data.insert(
        "description".into(),
        unescape(scan.extr("property=\"og:description\" content=\"", "\"")).into(),
    );
    let crumbs = split_html(scan.extr("cookiecrumb'>", "</span"));
    let path: Vec<Value> = crumbs
        .get(4..crumbs.len().saturating_sub(1))
        .unwrap_or_default()
        .iter()
        .step_by(2)
        .map(|p| Value::from(p.as_str()))
        .collect();
    data.insert("path".into(), path.into());
    data.insert(
        "date".into(),
        date(strptime(&modified, "%a, %d %b %Y %H:%M:%S GMT")),
    );
    data.insert("size".into(), parse_int(&header("content-length")).into());
    data.insert(
        "views".into(),
        parse_int(&scan.extr("Views</b>:", "<").replace(',', "")).into(),
    );
    data.insert(
        "width".into(),
        parse_int(scan.extr("Resolution</b>:", "x")).into(),
    );
    data.insert("height".into(), parse_int(scan.extr("", "<")).into());
    data.insert(
        "comments".into(),
        parse_int(scan.extr("Comments</b>:", "<")).into(),
    );
    data.insert(
        "favorites".into(),
        parse_int(scan.extr("Favorites</b>:", "<")).into(),
    );
    data.insert(
        "tags".into(),
        split_html(scan.extr("class='taglist'>", "</span>")).into(),
    );
    data.insert("filename".into(), name.into());
    data.insert("extension".into(), ext.into());
    data.extend(extra.clone());
    let file = Url::parse(&url).map_err(|_| error("invalid Eka's Portal URL"))?;
    Ok(Post::File(Box::new(
        Item::new(file, data).ids(post_id, post_id),
    )))
}

/// Post ids of a paginated listing (`_pagination_params`).
async fn ids_page(
    http: &Http,
    url: &str,
    params: &[(String, String)],
    page: i64,
    needle: &str,
    quote: &str,
) -> crate::Result<(Vec<String>, bool)> {
    let mut request = http.api(url)?;
    {
        let mut pairs = request.query_pairs_mut();
        pairs.extend_pairs(params.iter());
        pairs.append_pair("p", &page.to_string());
    }
    let html = http.text(&request).await?;
    let ids: Vec<String> = extract_iter(&html, needle, quote)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let more = ids.len() >= 40 || html.contains(">Next &gt;&gt;<");
    Ok((ids, more))
}

/// Posts, queueing folder contents when `recursive`.
fn posts_events<'a>(
    http: &'a Http,
    route: &'static Route,
    ids: Vec<String>,
    user: Option<String>,
    extra: Map<String, Value>,
    recursive: bool,
) -> Events<'a> {
    crate::sites::each_stream(ids, move |post_id| {
        let user = user.clone();
        let extra = extra.clone();
        async move {
            match parse_post(http, &post_id, user.as_deref(), &extra).await? {
                Post::File(item) => Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    route,
                    vec![*item],
                ))),
                Post::Folder if recursive => {
                    let base = format!("{ROOT}/g4/view/{post_id}");
                    let mut children = Vec::new();
                    for page in 1.. {
                        let (ids, more) =
                            ids_page(http, &base, &[], page, "class='gallery-item' id='", "'")
                                .await?;
                        children.extend(ids.into_iter().filter_map(|id| {
                            Some((
                                Url::parse(&format!("{ROOT}/g4/view/{id}")).ok()?,
                                Map::new(),
                            ))
                        }));
                        if !more {
                            break;
                        }
                    }
                    Ok(crate::sites::galleries::queue(http, &SITE, route, children))
                }
                _ => Ok(crate::sites::events(Vec::new())),
            }
        }
    })
}

/// `_pagination_params` pages turned into post events.
fn params_listing<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    params: Vec<(String, String)>,
    (needle, quote): (&'static str, &'static str),
    user: Option<String>,
    extra: Map<String, Value>,
) -> Events<'a> {
    let recursive = http.option("recursive").is_none_or(|v| truthy(Some(v)));
    let first = params
        .iter()
        .find(|(k, _)| k == "p")
        .map_or(1, |(_, v)| parse_int(v).max(1));
    let params: Vec<(String, String)> = params.into_iter().filter(|(k, _)| k != "p").collect();
    crate::sites::paginate(Some(first), move |state| {
        let url = url.clone();
        let params = params.clone();
        let user = user.clone();
        let extra = extra.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            let (ids, more) = ids_page(http, &url, &params, page, needle, quote).await?;
            Ok(Some((
                posts_events(http, route, ids, user, extra, recursive),
                more.then_some(page + 1),
            )))
        }
    })
}

/// `_pagination_next`: thumbnails across "Next >>" links.
fn next_listing<'a>(
    http: &'a Http,
    route: &'static Route,
    first: String,
    user: Option<String>,
    extra: Map<String, Value>,
) -> Events<'a> {
    let recursive = http.option("recursive").is_none_or(|v| truthy(Some(v)));
    crate::sites::paginate(Some(first), move |state| {
        let user = user.clone();
        let extra = extra.clone();
        async move {
            let Some(url) = state else { return Ok(None) };
            let page = http.text(&http.api(&url)?).await?;
            let ids = extract_iter(&page, "thumb' href='/g4/view/", "'")
                .into_iter()
                .map(str::to_owned)
                .collect();
            let next = page
                .find("Next &gt;&gt;")
                .map(|pos| format!("{ROOT}{}", rextr(&page, "href='", "'", Some(pos))));
            Ok(Some((
                posts_events(http, route, ids, user, extra, recursive),
                next,
            )))
        }
    })
}

/// `_pagination_folders`: favorites, descending into folders.
fn folders<'a>(http: &'a Http, user: String, folder: String) -> Events<'a> {
    let recursive = http.option("recursive").is_none_or(|v| truthy(Some(v)));
    let base = format!("{ROOT}/g4/favorites/{user}");
    let start = if folder.is_empty() {
        (base.clone(), String::new())
    } else {
        (format!("{base}/{folder}"), unquote(&folder))
    };
    // State: pending (url, folder name) frames, the current page and seen folders.
    crate::sites::paginate(
        Some((
            vec![start],
            1i64,
            std::collections::HashSet::<String>::new(),
        )),
        move |state| {
            let user = user.clone();
            async move {
                let Some((mut stack, page, mut seen)) = state else {
                    return Ok(None);
                };
                let Some((url, name)) = stack.last().cloned() else {
                    return Ok(None);
                };
                let html = http.text(&http.api(&format!("{url}?p={page}"))?).await?;
                let entries = extract_iter(&html, "<li class='gallery-item", "</li>");
                let more = entries.len() >= 40 || html.contains(">Next &gt;&gt;<");
                let mut ids = Vec::new();
                let mut subfolders = Vec::new();
                for item in entries {
                    if extr(item, "data-item-type=\"", "\"") == "Folders" {
                        let sub = rpartition(extr(item, "href='", "'"), "/").2.to_owned();
                        if recursive && seen.insert(sub.clone()) {
                            subfolders.push((format!("{url}/{sub}"), unquote(&sub)));
                        }
                    } else {
                        ids.push(extr(item, "data-item-id='", "'").to_owned());
                    }
                }
                let next_page = if more {
                    page + 1
                } else {
                    stack.pop();
                    1
                };
                stack.extend(subfolders.into_iter().rev());
                let mut extra = Map::new();
                extra.insert("folder".into(), name.into());
                let next = (!stack.is_empty()).then_some((stack, next_page, seen));
                Ok(Some((
                    posts_events(http, &FAVORITE, ids, Some(user), extra, recursive),
                    next,
                )))
            }
        },
    )
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Eka's Portal URL"));
    };
    crate::sites::once(async move {
        login(http).await?;
        let recursive = http.option("recursive").is_none_or(|v| truthy(Some(v)));
        let pairs = |query: &str| -> Vec<(String, String)> {
            parse_query(query)
                .into_iter()
                .map(|(k, v)| (k, crate::sites::scalar(&v)))
                .collect()
        };
        Ok(match target {
            Target::Post(id) => posts_events(http, &POST, vec![id], None, Map::new(), true),
            Target::Gallery(user) if recursive => params_listing(
                http,
                &GALLERY,
                format!("{ROOT}/g4/gallery/{user}"),
                Vec::new(),
                ("class='gallery-item' id='", "'"),
                Some(user),
                Map::new(),
            ),
            Target::Gallery(user) => next_listing(
                http,
                &GALLERY,
                format!("{ROOT}/g4/latest.php?name={user}"),
                Some(user),
                Map::new(),
            ),
            Target::Favorite(user, folder) => folders(http, user, folder),
            Target::Watch => {
                let base = http.api(ROOT)?;
                if http.cookie(&base, "phpbb3_rl7a3_sid").is_none() {
                    return Err(error(
                        "username & password or authenticated cookies required for watched Submissions",
                    ));
                }
                http.set_cookie(&base, "g4p_msgpage_style=plain; Path=/");
                params_listing(
                    http,
                    &WATCH,
                    format!("{ROOT}/g4/messagepage.php"),
                    Vec::new(),
                    ("data-item-id=\"", "\""),
                    None,
                    Map::new(),
                )
            }
            Target::Tag(query) => {
                let params = pairs(&query);
                let mut extra = Map::new();
                extra.insert(
                    "search_tags".into(),
                    params
                        .iter()
                        .find(|(k, _)| k == "tag")
                        .map_or(Value::Null, |(_, v)| v.as_str().into()),
                );
                params_listing(
                    http,
                    &TAG,
                    format!("{ROOT}/g4/tags.php"),
                    params,
                    ("class='gallery-item' id='", "'"),
                    None,
                    extra,
                )
            }
            Target::Search(query) => {
                let mut search: Map<String, Value> = parse_query(&query);
                let has = |key: &str| search.get(key).is_some_and(|v| truthy(Some(v)));
                let prefix = if has("q") {
                    ""
                } else if has("tags") {
                    "t_"
                } else if has("user") {
                    "u_"
                } else {
                    ""
                };
                search.insert("prefix".into(), prefix.into());
                let mut extra = Map::new();
                extra.insert("search".into(), Value::Object(search));
                next_listing(
                    http,
                    &SEARCH,
                    format!("{ROOT}/g4/search.php?{query}"),
                    None,
                    extra,
                )
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, response, serve};

    #[test]
    fn classifies_urls_and_dispositions() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://aryion.com/g4/gallery/jameshoward"),
            Some(Target::Gallery("jameshoward".into()))
        );
        assert_eq!(
            t("https://aryion.com/g4/favorites/jameshoward/Folder%201"),
            Some(Target::Favorite("jameshoward".into(), "Folder%201".into()))
        );
        assert_eq!(
            t("https://aryion.com/g4/search.php?q=a&tags=b"),
            Some(Target::Search("q=a&tags=b".into()))
        );
        assert_eq!(
            t("https://aryion.com/g4/view/510079"),
            Some(Target::Post("510079".into()))
        );
        assert_eq!(
            filename_from_contentdisposition("attachment; filename=\"a b.png\""),
            "a b.png"
        );
        assert_eq!(
            filename_from_contentdisposition(
                "inline; filename*=UTF-8''%E3%81%82.jpg; filename=\"x.jpg\""
            ),
            "あ.jpg"
        );
    }

    #[tokio::test]
    async fn post_uses_head_metadata() {
        let head = response(
            "200 OK\r\nContent-Disposition: inline; filename=\"Dragon art.png\"\r\nLast-Modified: Sat, 26 Mar 2022 16:2049 GMT\r\nContent-Length: 1234",
            "image/png",
            b"",
        );
        let page = r#"<title>g4 :: Dragon art by jameshoward</title><meta property="og:description" content="Desc">
            <span class='cookiecrumb'><a>Home</a> &raquo; <a>Gallery</a> &raquo; <a>Art</a> &raquo; <a>Dragons</a> &raquo; <a>Dragon art</a></span>
            Views</b>: 1,024<br>Resolution</b>: 800x600<br>"#;
        let (root, server) = serve(vec![head, html(page)]);
        let http = client("aryion", root);
        let files = media(extract(
            &http,
            Url::parse("https://aryion.com/g4/view/510079").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["user"].as_str(),
                k["width"].as_i64(),
                k["views"].as_i64()
            ),
            (
                Some("Dragon art"),
                Some("jameshoward"),
                Some(800),
                Some(1024)
            )
        );
        assert_eq!(k["date"], "2022-03-26 16:20:49");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "510079 Dragon art.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("aryion/jameshoward/Art/Dragons")
        );
        server.join().unwrap();
    }
}
