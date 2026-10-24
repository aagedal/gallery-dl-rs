//! Cyberfile folders, shares and files, mirroring
//! gallery_dl/extractor/cyberfile.py: listings come from the
//! `load_files` AJAX endpoint and queue folder and file pages; each file's
//! details come from `file_details`.
use super::{Body, Extr, request};
use crate::sites::art::split_html;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "cyberfile",
    name: "Cyberfile",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE, &FOLDER, &SHARED],
    api_key: false,
    cookies: Some("cyberfile.me"),
};

static FOLDER: Route = Route::new("folder", "{filename}.{extension}", &["{category}"], "");
static SHARED: Route = Route::new("shared", "{filename}.{extension}", &["{category}"], "");
static FILE: Route = Route::new(
    "file",
    "{filename}.{extension}",
    &["{category}", "{uploader}", "{folder}"],
    "",
);

const ROOT: &str = "https://cyberfile.me";

#[derive(Debug, PartialEq)]
enum Target {
    Folder(String),
    Shared(String),
    File(String),
}

/// The leading run of `text` matching `accept`.
fn run(text: &str, accept: fn(u8) -> bool) -> &str {
    &text[..text.bytes().position(|b| !accept(b)).unwrap_or(text.len())]
}

/// The upstream classes in order: `/folder/([0-9a-f]+)`,
/// `/shared/([a-zA-Z0-9]+)`, then any `/([a-zA-Z0-9]+)`.
fn target(url: &Url) -> Option<Target> {
    if !super::on_host(url, &["cyberfile.me"]) {
        return None;
    }
    let path = url.path();
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    if let Some(hash) = path.strip_prefix("/folder/").map(|rest| run(rest, hex))
        && !hash.is_empty()
    {
        return Some(Target::Folder(hash.to_owned()));
    }
    if let Some(id) = path
        .strip_prefix("/shared/")
        .map(|rest| run(rest, |b| b.is_ascii_alphanumeric()))
        && !id.is_empty()
    {
        return Some(Target::Shared(id.to_owned()));
    }
    let id = run(&path[1..], |b| b.is_ascii_alphanumeric());
    (!id.is_empty()).then(|| Target::File(id.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Folder(hash)) => folder(http, hash),
        Some(Target::Shared(id)) => crate::sites::once(async move { shared(http, &id).await }),
        Some(Target::File(id)) => crate::sites::once(async move {
            let item = file(http, &id).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &FILE,
                vec![item],
            )))
        }),
        None => crate::sites::fail(error("unsupported Cyberfile URL")),
    }
}

/// `request_api`: an AJAX form POST, answering a folder password prompt
/// with the `password` option.
async fn request_api(http: &Http, endpoint: &str, data: &[(&str, &str)]) -> Result<Value> {
    let url = http.api(&format!("{ROOT}{endpoint}"))?;
    let headers = [("X-Requested-With", "XMLHttpRequest"), ("Origin", ROOT)];
    let response = http.post_form(&url, data, &headers).await?;
    let javascript = response
        .get("javascript")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !javascript.contains("albumPasswordModel") {
        return Ok(response);
    }
    let Some(password) = http.option_str("password").filter(|p| !p.is_empty()) else {
        return Err(error("Cyberfile folder requires a password"));
    };
    let html = response
        .get("html")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let folder = crate::sites::art::extr(html, "<input type=\"hidden\" value=\"", "\"");
    let form = [
        ("folderPassword", password),
        ("folderId", folder),
        ("submitme", "1"),
    ];
    let url_pw = http.api(&format!("{ROOT}/ajax/folder_password_process"))?;
    let check = http.post_form(&url_pw, &form, &headers).await?;
    if !crate::sites::art::truthy(check.get("success")) {
        return Err(error("Cyberfile refused the folder password"));
    }
    http.post_form(&url, data, &headers).await
}

fn html(response: &Value) -> &str {
    response
        .get("html")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Queue the listed URLs that match `kind`.
fn queue<'a>(
    http: &'a Http,
    subcategory: &'static str,
    links: Vec<&str>,
    kind: fn(&Target) -> bool,
    data: &Map<String, Value>,
) -> Events<'a> {
    let children = links
        .into_iter()
        .filter_map(|link| Url::parse(&crate::sites::unescape(link)).ok())
        .filter(|url| target(url).as_ref().is_some_and(kind))
        .map(|url| (url, data.clone()))
        .collect();
    super::queue_all(http, SITE.category, subcategory, children)
}

fn is_folder(target: &Target) -> bool {
    matches!(target, Target::Folder(_))
}

fn is_file(target: &Target) -> bool {
    matches!(target, Target::File(_))
}

/// `sharing-url="..."` and `dtfullurl="..."` links (`[^"]+`).
fn links<'t>(html: &'t str, begin: &str) -> Vec<&'t str> {
    super::extract_iter(html, begin, "\"")
        .into_iter()
        .filter(|link| !link.is_empty())
        .collect()
}

const PER_PAGE: usize = 600;

/// `CyberfileFolderExtractor.items`: subfolders (with `recursive`) and
/// files of each listing page.
fn folder(http: &Http, hash: String) -> Events<'_> {
    let start = (1usize, None::<(String, Map<String, Value>)>);
    crate::sites::paginate(start, move |(page, state)| {
        let hash = hash.clone();
        async move {
            if page == 0 {
                return Ok(None);
            }
            let (num, mut data) = match state {
                Some(state) => state,
                None => {
                    let url = http.api(&format!("{ROOT}/folder/{hash}"))?;
                    let num =
                        crate::sites::art::extr(&http.text(&url).await?, "ages('folder', '", "'")
                            .to_owned();
                    let mut data = Map::new();
                    data.insert("folder_hash".into(), hash.into());
                    data.insert("folder_num".into(), super::parse_int(&num).into());
                    (num, data)
                }
            };
            let start = page.to_string();
            let per_page = PER_PAGE.to_string();
            let form = [
                ("pageType", "folder"),
                ("nodeId", num.as_str()),
                ("pageStart", start.as_str()),
                ("perPage", per_page.as_str()),
                ("filterOrderBy", ""),
            ];
            let response = request_api(http, "/account/ajax/load_files", &form).await?;
            if page == 1 {
                let title = response.get("page_title").cloned().unwrap_or(Value::Null);
                data.insert("folder".into(), title);
            }
            let html = html(&response);
            let folders = links(html, "sharing-url=\"");
            let files = links(html, "dtfullurl=\"");
            let more = folders.len() + files.len() >= PER_PAGE;
            let mut events = Vec::new();
            if http.option_bool("recursive", true) && !folders.is_empty() {
                events.push(queue(http, FOLDER.subcategory, folders, is_folder, &data));
            }
            events.push(queue(http, FOLDER.subcategory, files, is_file, &data));
            let next = if more { page + 1 } else { 0 };
            Ok(Some((
                futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                    futures_util::stream::iter(events),
                )),
                (next, Some((num, data))),
            )))
        }
    })
}

/// `CyberfileSharedExtractor.items`: the share's folders, then its files.
async fn shared<'a>(http: &'a Http, id: &str) -> Result<Events<'a>> {
    // The `filehosting` session cookie.
    let url = http.api(&format!("{ROOT}/shared/{id}"))?;
    let response = request(http, reqwest::Method::HEAD, &url, &[], Body::None).await?;
    for cookie in &response.cookies {
        http.set_cookie(&url, cookie);
    }
    let form = [
        ("pageType", "nonaccountshared"),
        ("nodeId", ""),
        ("pageStart", "1"),
        ("perPage", "500"),
        ("filterOrderBy", ""),
    ];
    let response = request_api(http, "/account/ajax/load_files", &form).await?;
    let html = html(&response);
    let pos = html
        .find("<!-- /.navbar-collapse -->")
        .map_or(25, |pos| pos + 26);
    let html = html.get(pos..).unwrap_or_default();
    let folders = links(html, "sharing-url=\"");
    let files = links(html, "dtfullurl=\"");
    let data = Map::new();
    Ok(futures_util::StreamExt::boxed(
        futures_util::StreamExt::chain(
            queue(http, SHARED.subcategory, folders, is_folder, &data),
            queue(http, SHARED.subcategory, files, is_file, &data),
        ),
    ))
}

/// `CyberfileFileExtractor.items`: the file page's number, then its
/// `file_details` panel.
async fn file(http: &Http, id: &str) -> Result<Item> {
    let page = http.text(&http.api(&format!("{ROOT}/{id}"))?).await?;
    let num = crate::sites::art::extr(&page, "owFileInformation(", ")").to_owned();
    let response = request_api(http, "/account/ajax/file_details", &[("u", &num)]).await?;
    let mut extr = Extr::new(html(&response));
    let info = split_html(extr.next("class=\"text-section\">", "</span>"));
    let folder = if info.len() > 1 { info[0].as_str() } else { "" };
    let uploader = info
        .last()
        .ok_or_else(|| error("Cyberfile file has no uploader"))?;
    let uploader = uploader.chars().skip(2).collect::<String>();
    let mut size = crate::sites::remove_html(extr.next("Filesize:", "</tr>"));
    size.pop();
    let tags = split_html(extr.next("Keywords:", "</tr>"));
    let date = crate::sites::art::strptime(
        &crate::sites::remove_html(extr.next("Uploaded:", "</tr>")),
        "%d/%m/%Y %H:%M:%S",
    );
    let permissions: Vec<Value> = crate::sites::remove_html(extr.next("Permissions:", "</tr>"))
        .split(" &amp; ")
        .map(Value::from)
        .collect();
    let url = extr.next("openUrl('", "'").to_owned();
    let name = response.get("page_title").cloned().unwrap_or(Value::Null);
    let mut data = Map::new();
    data.insert("file_id".into(), id.into());
    data.insert("file_num".into(), super::parse_int(&num).into());
    data.insert("name".into(), name.clone());
    data.insert("folder".into(), folder.into());
    data.insert("uploader".into(), uploader.trim().into());
    data.insert("size".into(), super::parse_bytes(&size).into());
    data.insert("tags".into(), tags.into());
    data.insert("date".into(), super::date_value(date));
    data.insert("permissions".into(), permissions.into());
    data.insert("file_url".into(), url.clone().into());
    let name = name
        .as_str()
        .filter(|name| !name.is_empty())
        .unwrap_or(&url);
    super::nameext_from_url(name, &mut data);
    let file =
        super::web_url(&url).ok_or_else(|| error("Cyberfile returned an invalid file URL"))?;
    Ok(Item::new(file, data).ids(id, id).timestamp(date))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{
        html as page, http, json, keywords, media, queued, response, serve,
    };
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://cyberfile.me/folder/0123abcdef/NAME"),
            Some(Target::Folder("0123abcdef".into()))
        );
        assert_eq!(
            t("https://www.cyberfile.me/shared/AbCdEf"),
            Some(Target::Shared("AbCdEf".into()))
        );
        assert_eq!(
            t("https://cyberfile.me/AbC12/x"),
            Some(Target::File("AbC12".into()))
        );
        assert_eq!(
            t("https://cyberfile.me/folder/XYZ"),
            Some(Target::File("folder".into()))
        );
        for raw in [
            "https://cyberfile.me/",
            "https://cyberfile.me.evil.test/abc",
            "https://cdn.cyberfile.me/abc",
            "https://cyberfile.me:8443/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const DETAILS: &str = "<span class=\"text-section\"><a>Pics</a> / <a>by Alice</a></span>\
        <tr><td>Filesize:</td><td>1.5 MB</td></tr>\
        <tr><td>Keywords:</td><td><a>one</a><a>two</a></td></tr>\
        <tr><td>Uploaded:</td><td>05/06/2024 12:30:00</td></tr>\
        <tr><td>Permissions:</td><td>Read &amp; Download</td></tr>\
        <button onclick=\"openUrl('https://cyberfile.me/download/AbC12/a.jpg'); return false;\">";

    #[tokio::test]
    async fn extracts_file_details() {
        let (root, server) = serve(vec![
            page("<script>showFileInformation(4242);</script>"),
            json(json!({"html": DETAILS, "page_title": "Holiday.JPG"})),
        ]);
        let http = http("cyberfile", root);
        let files = media(extract(
            &http,
            Url::parse("https://cyberfile.me/AbC12").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://cyberfile.me/download/AbC12/a.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["file_num"], 4242);
        assert_eq!(k["folder"], "Pics");
        assert_eq!(k["uploader"], "Alice");
        assert_eq!(k["size"], 1_572_864);
        assert_eq!(k["tags"], json!(["one", "two"]));
        assert_eq!(k["date"], "2024-06-05 12:30:00");
        assert_eq!(k["permissions"], json!(["Read", "Download"]));
        assert_eq!(k["filename"], "Holiday");
        assert_eq!(k["extension"], "jpg");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Holiday.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("cyberfile/Alice/Pics")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["",[]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /AbC12 "));
        assert!(requests[1].starts_with("POST /account/ajax/file_details "));
        assert!(requests[1].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[1].ends_with("\r\n\r\nu=4242"));
    }

    #[tokio::test]
    async fn folders_queue_subfolders_and_files() {
        let listing = "<a sharing-url=\"https://cyberfile.me/folder/beef/Sub\">\
            <a dtfullurl=\"https://cyberfile.me/Xy1\"><a dtfullurl=\"https://evil.test/Xy2\">";
        let (root, server) = serve(vec![
            page("loadImages('folder', '77');"),
            json(json!({"html": listing, "page_title": "Root"})),
        ]);
        let http = http("cyberfile", root);
        let children = queued(extract(
            &http,
            Url::parse("https://cyberfile.me/folder/abc1").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "folder");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /folder/abc1 "));
        assert!(
            requests[1]
                .ends_with("pageType=folder&nodeId=77&pageStart=1&perPage=600&filterOrderBy=")
        );
    }

    #[tokio::test]
    async fn shares_take_the_session_cookie() {
        let head = response(
            "200 OK\r\nSet-Cookie: filehosting=s1; Path=/",
            "text/html",
            b"",
        );
        let listing = "<a sharing-url=\"https://cyberfile.me/folder/0/x\"><!-- /.navbar-collapse -->\
            <a dtfullurl=\"https://cyberfile.me/F1\">";
        let (root, server) = serve(vec![head, json(json!({"html": listing}))]);
        let http = http("cyberfile", root);
        let children = queued(extract(
            &http,
            Url::parse("https://cyberfile.me/shared/S1").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "shared");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("HEAD /shared/S1 "));
        assert!(requests[1].contains("cookie: filehosting=s1"));
        assert!(
            requests[1].ends_with(
                "pageType=nonaccountshared&nodeId=&pageStart=1&perPage=500&filterOrderBy="
            )
        );
    }

    #[tokio::test]
    async fn password_prompt_without_option_fails() {
        let (root, _server) = serve(vec![
            page("x"),
            json(json!({"javascript": "$('#albumPasswordModel').modal()", "html": ""})),
        ]);
        let http = http("cyberfile", root);
        let events = extract(&http, Url::parse("https://cyberfile.me/abc").unwrap());
        assert!(crate::sites::testing::fails(events).await);
    }
}
