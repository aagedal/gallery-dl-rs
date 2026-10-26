//! Cyberfile files, folders and shares, mirroring
//! gallery_dl/extractor/cyberfile.py. Protected folders use the `password`
//! option.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_bytes, parse_int, re,
    remove_html, split_html, str_of, strptime,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://cyberfile.me";

static FILE: Route = Route::new(
    "file",
    "{filename}.{extension}",
    &["{category}", "{uploader}", "{folder}"],
    "",
);
static FOLDER: Route = Route::new("folder", "{filename}.{extension}", &["{category}"], "");
static SHARED: Route = Route::new("shared", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "cyberfile",
    name: "Cyberfile",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE, &FOLDER, &SHARED],
    api_key: false,
    cookies: Some("cyberfile.me"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Folder(String),
    Shared(String),
    File(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?cyberfile\.me";
    if let Some(c) = captures(re!(&format!(r"{BASE}/folder/([0-9a-f]+)")), url) {
        return Some(Target::Folder(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/shared/([a-zA-Z0-9]+)")), url) {
        return Some(Target::Shared(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/([a-zA-Z0-9]+)")), url)
        .map(|c| Target::File(group(&c, 1).into()))
}

/// `request_api`, answering a folder password prompt once.
async fn api(http: &Http, endpoint: &str, form: &[(&str, &str)]) -> crate::Result<Value> {
    let headers = [("X-Requested-With", "XMLHttpRequest"), ("Origin", ROOT)];
    let url = http.api(&format!("{ROOT}{endpoint}"))?;
    let resp = http.post_form(&url, form, &headers).await?;
    if !str_of(&resp, "javascript").contains("albumPasswordModel") {
        return Ok(resp);
    }
    let password = http
        .option("password")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let folder_id = extr(
        str_of(&resp, "html"),
        "<input type=\"hidden\" value=\"",
        "\"",
    )
    .to_owned();
    let unlock = http
        .post_form(
            &http.api(&format!("{ROOT}/ajax/folder_password_process"))?,
            &[
                ("folderPassword", &password),
                ("folderId", &folder_id),
                ("submitme", "1"),
            ],
            &headers,
        )
        .await?;
    if !crate::sites::gdl::truthy(unlock.get("success")) {
        return Err(error(format!(
            "'{}'",
            unlock
                .get("msg")
                .map(crate::sites::scalar)
                .unwrap_or_default()
        )));
    }
    http.post_form(&url, form, &headers).await
}

fn queue<'a>(
    http: &'a Http,
    route: &'static Route,
    urls: Vec<String>,
    data: &Map<String, Value>,
) -> Events<'a> {
    let data = data.clone();
    futures_util::stream::iter(urls.into_iter().filter_map(|u| Url::parse(&u).ok()))
        .map(move |url| crate::sites::queue(http, SITE.category, route.subcategory, url, &data))
        .flatten()
        .boxed()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Cyberfile URL"));
    };
    match target {
        Target::Folder(hash) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/folder/{hash}"))?)
                .await?;
            let num = extr(&page, "ages('folder', '", "'").to_owned();
            let recursive = http.option_bool("recursive", true);
            const PER_PAGE: usize = 600;
            Ok(crate::sites::paginate(
                Some((1usize, None::<Map<String, Value>>)),
                move |state| {
                    let hash = hash.clone();
                    let num = num.clone();
                    async move {
                        let Some((start, folder)) = state else {
                            return Ok(None);
                        };
                        let start_text = start.to_string();
                        let per_page = PER_PAGE.to_string();
                        let form = [
                            ("pageType", "folder"),
                            ("nodeId", num.as_str()),
                            ("pageStart", start_text.as_str()),
                            ("perPage", per_page.as_str()),
                            ("filterOrderBy", ""),
                        ];
                        let resp = api(http, "/account/ajax/load_files", &form).await?;
                        let folder = folder.unwrap_or_else(|| {
                            let mut folder = Map::new();
                            folder.insert("folder_hash".into(), hash.as_str().into());
                            folder.insert("folder_num".into(), parse_int(&num).into());
                            folder.insert("folder".into(), resp["page_title"].clone());
                            folder
                        });
                        let html = str_of(&resp, "html");
                        let folders: Vec<String> = re!(r#"sharing-url="([^"]+)"#)
                            .captures_iter(html)
                            .map(|c| group(&c, 1).to_owned())
                            .collect();
                        let files: Vec<String> = re!(r#"dtfullurl="([^"]+)"#)
                            .captures_iter(html)
                            .map(|c| group(&c, 1).to_owned())
                            .collect();
                        let more = folders.len() + files.len() >= PER_PAGE;
                        let mut events = Vec::new();
                        if recursive && !folders.is_empty() {
                            events.push(queue(http, &FOLDER, folders, &folder));
                        }
                        events.push(queue(http, &FOLDER, files, &folder));
                        let events = futures_util::stream::iter(events).flatten().boxed();
                        Ok(Some((events, more.then_some((start + 1, Some(folder))))))
                    }
                },
            ))
        }),
        Target::Shared(id) => crate::sites::once(async move {
            // Sets the `filehosting` session cookie.
            http.get(&http.api(&format!("{ROOT}/shared/{id}"))?, &[])
                .await?;
            let form = [
                ("pageType", "nonaccountshared"),
                ("nodeId", ""),
                ("pageStart", "1"),
                ("perPage", "500"),
                ("filterOrderBy", ""),
            ];
            let resp = api(http, "/account/ajax/load_files", &form).await?;
            let html = str_of(&resp, "html");
            let pos = html
                .find("<!-- /.navbar-collapse -->")
                .map_or(25, |i| i + 26)
                .min(html.len());
            let html = &html[pos..];
            let folders = extract_iter(html, "sharing-url=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            let files = extract_iter(html, "dtfullurl=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            Ok(queue(http, &SHARED, folders, &Map::new())
                .chain(queue(http, &SHARED, files, &Map::new()))
                .boxed())
        }),
        Target::File(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/{id}"))?).await?;
            let num = extr(&page, "owFileInformation(", ")").to_owned();
            let resp = api(http, "/account/ajax/file_details", &[("u", &num)]).await?;
            let mut scan = Scan::new(str_of(&resp, "html"));
            let info = split_html(scan.extr("class=\"text-section\">", "</span>"));
            let folder = if info.len() > 1 {
                info[0].clone()
            } else {
                String::new()
            };
            let uploader = info
                .last()
                .map(|u| u.chars().skip(2).collect::<String>().trim().to_owned())
                .unwrap_or_default();
            let size = remove_html(scan.extr("Filesize:", "</tr>"));
            let mut data = Map::new();
            data.insert("file_id".into(), id.as_str().into());
            data.insert("file_num".into(), parse_int(&num).into());
            data.insert("name".into(), resp["page_title"].clone());
            data.insert("folder".into(), folder.into());
            data.insert("uploader".into(), uploader.into());
            data.insert(
                "size".into(),
                parse_bytes(size.get(..size.len().saturating_sub(1)).unwrap_or_default()).into(),
            );
            data.insert(
                "tags".into(),
                split_html(scan.extr("Keywords:", "</tr>")).into(),
            );
            data.insert(
                "date".into(),
                date(strptime(
                    &remove_html(scan.extr("Uploaded:", "</tr>")),
                    "%d/%m/%Y %H:%M:%S",
                )),
            );
            let permissions: Vec<String> = remove_html(scan.extr("Permissions:", "</tr>"))
                .split(" &amp; ")
                .map(str::to_owned)
                .collect();
            data.insert("permissions".into(), permissions.into());
            let raw = scan.extr("openUrl('", "'").to_owned();
            data.insert("file_url".into(), raw.as_str().into());
            let name = str_of(&resp, "page_title");
            nameext_into(if name.is_empty() { &raw } else { name }, &mut data);
            let file = Url::parse(&raw).map_err(|_| error("Cyberfile file has no download URL"))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &FILE,
                vec![Item::new(file, data).ids(id.clone(), id)],
            )))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://cyberfile.me/folder/82d0aab0853fdd13/Playlist"),
            Some(Target::Folder("82d0aab0853fdd13".into()))
        );
        assert_eq!(
            t("https://cyberfile.me/shared/tao35avvfc"),
            Some(Target::Shared("tao35avvfc".into()))
        );
        assert_eq!(
            t("https://cyberfile.me/bpfD"),
            Some(Target::File("bpfD".into()))
        );
    }

    #[tokio::test]
    async fn extracts_file_details() {
        let details = json!({"page_title": "clip.mp4", "html": "<span class=\"text-section\"><a>Folder A</a> <a>by Uploader</a></span>\
            <tr><td>Filesize:</td><td>1.5MB</td></tr><tr><td>Keywords:</td><td><a>a</a><a>b</a></td></tr>\
            <tr><td>Uploaded:</td><td>02/03/2024 10:11:12</td></tr><tr><td>Permissions:</td><td>Download &amp; Share</td></tr>\
            <button onclick=\"openUrl('https://cyberfile.me/download/abc'); return false;\">"});
        let (root, server) = serve(vec![html("showFileInformation(1234);"), json(details)]);
        let http = client("cyberfile", root);
        let files = media(extract(
            &http,
            Url::parse("https://cyberfile.me/bpfD").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://cyberfile.me/download/abc");
        let k = keywords(&files[0]);
        assert_eq!(k["folder"], "Folder A");
        assert_eq!(k["uploader"], "Uploader");
        assert_eq!(k["size"], 1572864);
        assert_eq!(k["date"], "2024-03-02 10:11:12");
        assert_eq!(k["permissions"], json!(["Download", "Share"]));
        assert_eq!(k["filename"], "clip");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /account/ajax/file_details "));
        assert!(requests[1].ends_with("u=1234"));
    }

    #[tokio::test]
    async fn unlocks_protected_folders() {
        let locked = json!({"javascript": "showModal('albumPasswordModel')", "html": "<input type=\"hidden\" value=\"55\">"});
        let listing = json!({"page_title": "Playlist", "html": "<a sharing-url=\"https://cyberfile.me/folder/ab12\"></a><tr dtfullurl=\"https://cyberfile.me/x1\">"});
        let (root, server) = serve(vec![
            html("loadImages('folder', '99')"),
            json(locked),
            json(json!({"success": true})),
            json(listing),
        ]);
        let http = client("cyberfile", root).with_options(Some(std::sync::Arc::new(
            json!({"password": "pw"}).as_object().unwrap().clone(),
        )));
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://cyberfile.me/folder/ab12cd").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1]
                .ends_with("pageType=folder&nodeId=99&pageStart=1&perPage=600&filterOrderBy=")
        );
        assert!(requests[2].ends_with("folderPassword=pw&folderId=55&submitme=1"));
    }
}
