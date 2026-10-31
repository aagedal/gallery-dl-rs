//! Misskey instances, mirroring gallery_dl/extractor/misskey.py through the
//! public `/api` endpoints.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

const FILENAME: &str = "{category}_{id}_{file[id]}.{extension}";
const DIRECTORY: &[&str] = &["misskey", "{instance}", "{user[username]}"];
const ARCHIVE: &str = "{id}_{file[id]}";
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static NOTES: Route = Route::new("notes", FILENAME, DIRECTORY, ARCHIVE);
static INFO: Route = Route::new("info", FILENAME, DIRECTORY, ARCHIVE);
static AVATAR: Route = Route::new("avatar", FILENAME, DIRECTORY, ARCHIVE);
static BACKGROUND: Route = Route::new("background", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);
static NOTE: Route = Route::new("note", FILENAME, DIRECTORY, ARCHIVE);
static FAVORITE: Route = Route::new("favorite", FILENAME, DIRECTORY, ARCHIVE);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| {
                extract(
                    Inst {
                        site: &$ident,
                        root: concat!("https://", $category),
                    },
                    http,
                    url,
                )
            },
            routes: &[
                &NOTE,
                &USER,
                &NOTES,
                &INFO,
                &AVATAR,
                &BACKGROUND,
                &FOLLOWING,
                &FAVORITE,
            ],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(MISSKEY_IO, "misskey.io", "Misskey.io");
instance!(MISSKEY_DESIGN, "misskey.design", "misskey.design");
instance!(MISSKEY_ART, "misskey.art", "misskey.art");
instance!(LESBIAN_ENERGY, "lesbian.energy", "lesbian.energy");
instance!(SUSHI_SKI, "sushi.ski", "sushi.ski");

/// `misskey:https://HOST/...`: any other Misskey instance.
pub static MISSKEY: Site = Site {
    category: "misskey",
    name: "Misskey",
    matches: |url| {
        crate::sites::custom_root(url, "misskey").is_some_and(|root| target(root, url).is_some())
    },
    extract: |http, url| match crate::sites::custom_root(&url, "misskey") {
        Some(root) => extract(
            Inst {
                site: &MISSKEY,
                root,
            },
            http,
            url,
        ),
        None => crate::sites::fail(error("unsupported Misskey URL")),
    },
    routes: MISSKEY_IO.routes,
    api_key: false,
    cookies: None,
};

/// A Misskey instance: its registry site and `https://HOST` root.
#[derive(Clone, Copy)]
struct Inst {
    site: &'static Site,
    root: &'static str,
}

impl Inst {
    fn host(self) -> &'static str {
        self.root
            .split_once("://")
            .map_or(self.root, |(_, host)| host)
    }

    /// A page URL on the instance, `misskey:`-prefixed for custom hosts.
    fn page(self, path: &str) -> String {
        if self.site.category == "misskey" {
            format!("misskey:{}{path}", self.root)
        } else {
            format!("{}{path}", self.root)
        }
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    User(String),
    Notes(String),
    Info(String),
    Avatar(String),
    Background(String),
    Following(String),
    Note(String),
    Favorite,
}

/// The instance host itself; upstream patterns have no `www.` alternative.
/// `category` is the instance host, or the root of a `misskey:` input.
fn target(category: &str, url: &Url) -> Option<Target> {
    let inner;
    let url = if crate::sites::is_custom_root(category) {
        inner = crate::sites::custom_domain(url, "misskey")?;
        &inner
    } else {
        url
    };
    if !crate::sites::plain(url)
        || !crate::sites::is_custom_root(category) && url.host_str() != Some(category)
    {
        return None;
    }
    let parts = crate::sites::segments(url);
    let p: Vec<&str> = parts.iter().map(String::as_str).collect();
    let user = |segment: &str| {
        segment
            .strip_prefix('@')
            .filter(|u| !u.is_empty())
            .map(str::to_owned)
    };
    match p.as_slice() {
        [first] if url.query().is_none() && url.fragment().is_none() => {
            user(first).map(Target::User)
        }
        [first, kind, ..] if first.starts_with('@') => {
            let name = user(first)?;
            match *kind {
                "notes" => Some(Target::Notes(name)),
                "info" => Some(Target::Info(name)),
                "avatar" => Some(Target::Avatar(name)),
                "banner" | "background" => Some(Target::Background(name)),
                "following" => Some(Target::Following(name)),
                _ => None,
            }
        }
        ["notes", id, ..]
            if id.chars().all(|c| c.is_alphanumeric() || c == '_') && !id.is_empty() =>
        {
            Some(Target::Note((*id).into()))
        }
        ["my", "favorites", ..] | ["api", "i", "favorites", ..] => Some(Target::Favorite),
        _ => None,
    }
}

fn root(site: Inst) -> String {
    site.root.to_owned()
}

async fn call(http: &Http, site: Inst, endpoint: &str, data: &Value) -> Result<Value> {
    let url = http.api(&format!("{}/api{endpoint}", root(site)))?;
    http.post_json(&url, data, &[]).await
}

async fn users_show(http: &Http, site: Inst, username: &str) -> Result<Value> {
    let (name, host) = username.split_once('@').unwrap_or((username, ""));
    let host = if host.is_empty() {
        Value::Null
    } else {
        host.into()
    };
    let user = call(
        http,
        site,
        "/users/show",
        &json!({"username": name, "host": host}),
    )
    .await?;
    user.is_object()
        .then_some(user)
        .ok_or_else(|| error("Misskey user response is not an object"))
}

fn extract<'a>(site: Inst, http: &'a Http, url: Url) -> Events<'a> {
    let key = if site.site.category == "misskey" {
        site.root
    } else {
        site.site.category
    };
    let Some(target) = target(key, &url) else {
        return crate::sites::fail(error("unsupported Misskey URL"));
    };
    match target {
        Target::User(user) => dispatch(site, http, &user),
        Target::Notes(user) => crate::sites::once(async move {
            let id = user_id(&users_show(http, site, &user).await?)?;
            Ok(paginate(
                site,
                http,
                &NOTES,
                "/users/notes",
                json!({"userId": id}),
            ))
        }),
        Target::Info(user) => crate::sites::once(async move {
            // Upstream yields only a Directory message with the profile.
            users_show(http, site, &user).await?;
            Ok(crate::sites::events(Vec::new()))
        }),
        Target::Avatar(user) => profile_image(site, http, user, &AVATAR, "avatar", "avatarUrl"),
        Target::Background(user) => {
            profile_image(site, http, user, &BACKGROUND, "background", "bannerUrl")
        }
        Target::Following(user) => crate::sites::once(async move {
            let id = user_id(&users_show(http, site, &user).await?)?;
            Ok(paginate(
                site,
                http,
                &FOLLOWING,
                "/users/following",
                json!({"userId": id}),
            ))
        }),
        Target::Note(id) => crate::sites::once(async move {
            let note = call(http, site, "/notes/show", &json!({"noteId": id})).await?;
            Ok(crate::sites::events(notes_events(
                site,
                http,
                &NOTE,
                vec![note],
            )))
        }),
        Target::Favorite => match http.option_str("access-token").filter(|t| !t.is_empty()) {
            Some(token) => paginate(site, http, &FAVORITE, "/i/favorites", json!({"i": token})),
            None => {
                crate::sites::fail(error("Misskey favorites require the 'access-token' option"))
            }
        },
    }
}

fn user_id(user: &Value) -> Result<String> {
    user.get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| error("Misskey user response has no id"))
}

/// `Dispatch._dispatch_extractors` with the default `include` of `notes`.
fn dispatch<'a>(site: Inst, http: &'a Http, user: &str) -> Events<'a> {
    let base = site.page(&format!("/@{user}/"));
    let all = ["info", "avatar", "background", "notes"];
    let include: Vec<String> = match http.option("include") {
        None => vec!["notes".into()],
        Some(Value::String(s)) if s == "all" => all.iter().map(|s| (*s).to_owned()).collect(),
        Some(Value::String(s)) => s.replace(' ', "").split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => Vec::new(),
    };
    let mut children = Vec::new();
    for name in include {
        let path = match name.as_str() {
            "info" => "info",
            "avatar" => "avatar",
            "background" => "banner",
            "notes" => "notes",
            _ => continue,
        };
        if let Ok(url) = Url::parse(&format!("{base}{path}")) {
            children.push(url);
        }
    }
    let category = site.site.category;
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(
            children
                .into_iter()
                .map(move |url| crate::sites::queue(http, category, "user", url, &Map::new())),
        ),
    ))
}

fn profile_image<'a>(
    site: Inst,
    http: &'a Http,
    user: String,
    route: &'static Route,
    kind: &'static str,
    key: &'static str,
) -> Events<'a> {
    crate::sites::once(async move {
        let user = users_show(http, site, &user).await?;
        let notes = match user
            .get(key)
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
        {
            Some(url) => vec![make_note(kind, &user, url)],
            None => Vec::new(),
        };
        Ok(crate::sites::events(notes_events(site, http, route, notes)))
    })
}

/// `_make_note`: a synthetic note for a profile image, unwrapping media
/// proxy URLs via their `url` parameter.
fn make_note(kind: &str, user: &Value, url: &str) -> Value {
    let url = match url.split_once('?') {
        Some((path, query)) => {
            crate::sites::media::query_get(&crate::sites::media::parse_query(Some(query)), "url")
                .filter(|u| !u.is_empty())
                .unwrap_or(path)
                .to_owned()
        }
        None => url.to_owned(),
    };
    let last = url.rsplit('/').next().unwrap_or_default();
    let id = last.split('.').next().unwrap_or_default();
    json!({
        "id": kind,
        "user": user,
        "files": [{"id": id, "url": url, "createdAt": ""}],
        "createdAt": "",
    })
}

/// `_pagination`: 100 notes per request, continued with `untilId`.
fn paginate<'a>(
    site: Inst,
    http: &'a Http,
    route: &'static Route,
    endpoint: &'static str,
    mut data: Value,
) -> Events<'a> {
    data["limit"] = 100.into();
    data["withRenotes"] = http.option_bool("renotes", false).into();
    data["withFiles"] = (!http.option_bool("text-posts", false)).into();
    crate::sites::paginate((Some(data), 0u32), move |(data, page)| async move {
        let Some(mut data) = data else {
            return Ok(None);
        };
        let notes = call(http, site, endpoint, &data).await?;
        let notes: Vec<Value> = notes
            .as_array()
            .cloned()
            .ok_or_else(|| error("Misskey page is not a list"))?;
        let Some(last) = notes.last().and_then(|n| n.get("id")).cloned() else {
            return Ok(None);
        };
        data["untilId"] = last;
        let next = (page < 100_000).then_some(data);
        let events = if endpoint == "/users/following" {
            following_events(site, http, notes)
        } else {
            crate::sites::events(notes_events(site, http, route, notes))
        };
        Ok(Some((events, (next, page + 1))))
    })
}

fn following_events<'a>(site: Inst, http: &'a Http, entries: Vec<Value>) -> Events<'a> {
    let mut children = Vec::new();
    for entry in entries {
        let Some(user) = entry.get("followee").and_then(Value::as_object) else {
            continue;
        };
        let name = user.get("username").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() || name.contains(['/', '?', '#', '@']) {
            continue;
        }
        let mut url = site.page(&format!("/@{name}"));
        if let Some(host) = user.get("host").and_then(Value::as_str) {
            if host.contains(['/', '?', '#', '@']) {
                continue;
            }
            url = format!("{url}@{host}");
        }
        if let Ok(url) = Url::parse(&url) {
            children.push((url, user.clone()));
        }
    }
    let category = site.site.category;
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(
            children.into_iter().map(move |(url, data)| {
                crate::sites::queue(http, category, "following", url, &data)
            }),
        ),
    ))
}

/// `items()`: one post per note with its own, renoted and replied files.
fn notes_events(
    site: Inst,
    http: &Http,
    route: &'static Route,
    notes: Vec<Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let renotes = http.option_bool("renotes", false);
    let replies = http.option_bool("replies", true);
    let mut events = Vec::new();
    for note in notes {
        let note = match note.get("note") {
            Some(inner) => inner.clone(),
            None => note,
        };
        let Some(mut note) = note.as_object().cloned() else {
            continue;
        };
        let mut files: Vec<Value> = match note.remove("files") {
            Some(Value::Array(files)) => files,
            _ => Vec::new(),
        };
        let truthy = |v: Option<&Value>| v.is_some_and(|v| !v.is_null() && v != &json!({}));
        if truthy(note.get("renote")) {
            if !renotes {
                continue;
            }
            files.extend(attached(&note["renote"]));
        }
        if truthy(note.get("reply")) {
            if !replies {
                continue;
            }
            files.extend(attached(&note["reply"]));
        }
        let host = note
            .get("user")
            .and_then(|u| u.get("host"))
            .cloned()
            .unwrap_or(Value::Null);
        note.insert("instance".into(), site.host().into());
        note.insert("instance_remote".into(), host);
        note.insert("count".into(), files.len().into());
        let created = crate::sites::media::iso_ts(crate::sites::media::str_of(
            &Value::Object(note.clone()),
            "createdAt",
        ));
        note.insert("date".into(), crate::sites::media::date(created));
        let note_id = note.get("id").map(crate::sites::scalar).unwrap_or_default();
        let username = note
            .get("user")
            .and_then(|u| u.get("username"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let caption = note.get("text").and_then(Value::as_str).map(str::to_owned);
        let mut items = Vec::new();
        for (num, file) in files.into_iter().enumerate() {
            let Some(mut file) = file.as_object().cloned() else {
                continue;
            };
            let Some(url) = file
                .get("url")
                .and_then(Value::as_str)
                .and_then(crate::sites::media::web_url)
            else {
                continue;
            };
            let file_ts = crate::sites::media::iso_ts(
                file.get("createdAt").and_then(Value::as_str).unwrap_or(""),
            );
            file.insert("date".into(), crate::sites::media::date(file_ts));
            let file_id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
            let mut keywords = note.clone();
            keywords.insert("num".into(), (num + 1).into());
            keywords.insert("file".into(), file.into());
            crate::sites::media::set_nameext(&mut keywords, crate::sites::nameext(&url));
            items.push(
                Item::new(url, keywords)
                    .ids(note_id.clone(), file_id)
                    .user(username.clone())
                    .caption(caption.clone())
                    .timestamp(created.or(file_ts)),
            );
        }
        events.extend(crate::sites::post(site.site, http, route, items));
    }
    events
}

fn attached(note: &Value) -> Vec<Value> {
    note.get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    const IO: Inst = Inst {
        site: &MISSKEY_IO,
        root: "https://misskey.io",
    };
    use super::*;
    use crate::sites::testing::{http, json as reply, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("misskey.io", "https://misskey.io/@lithla"),
            Some(Target::User("lithla".into()))
        );
        assert_eq!(
            t("misskey.io", "https://misskey.io/@user@other.host/notes"),
            Some(Target::Notes("user@other.host".into()))
        );
        assert_eq!(
            t("sushi.ski", "https://sushi.ski/@u/banner"),
            Some(Target::Background("u".into()))
        );
        assert_eq!(
            t("misskey.art", "https://misskey.art/notes/9bhqfo835v"),
            Some(Target::Note("9bhqfo835v".into()))
        );
        assert_eq!(
            t("misskey.io", "https://misskey.io/api/i/favorites"),
            Some(Target::Favorite)
        );
        for raw in [
            "https://www.misskey.io/@u",
            "https://misskey.io.evil.test/@u",
            "https://misskey.io:8443/@u",
            "https://misskey.io/@u?x=1",
            "https://misskey.io/about",
            "https://misskey.io/@u/clips",
        ] {
            assert_eq!(t("misskey.io", raw), None, "{raw}");
        }
        assert!(
            crate::sites::find(&Url::parse("https://lesbian.energy/@u/notes").unwrap())
                .is_some_and(|site| site.category == "lesbian.energy")
        );
    }

    fn note(id: &str, renote: bool) -> Value {
        json!({"id": id, "createdAt": "2017-07-14T02:40:00.000Z", "text": "hi",
            "user": {"id": "u1", "username": "alice", "host": null},
            "renote": if renote { json!({"files": [{"id": "r"}]}) } else { Value::Null },
            "files": [{"id": format!("f{id}"), "createdAt": "2017-07-14T02:40:01.000Z",
                       "url": format!("https://media.misskeyusercontent.jp/io/{id}.webp")}]})
    }

    #[tokio::test]
    async fn notes_paginate_with_until_id_and_skip_renotes() {
        let (root, server) = serve(vec![
            reply(json!({"id": "u1", "username": "alice"})),
            reply(json!([note("n1", false), note("n2", true)])),
            reply(json!([])),
        ]);
        let http = http("misskey.io", root);
        let files = media(extract(
            IO,
            &http,
            Url::parse("https://misskey.io/@alice/notes").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["instance"], "misskey.io");
        assert_eq!(k["instance_remote"], Value::Null);
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["file"]["date"], "2017-07-14 02:40:01");
        assert_eq!(k["num"], 1);
        assert_eq!(k["count"], 1);
        assert!(!k.contains_key("files"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "misskey.io_n1_fn1.webp");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("misskey/misskey.io/alice")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}_{file[id]}",["n1","fn1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/users/show "));
        assert!(requests[0].contains(r#""username":"alice""#));
        assert!(requests[1].starts_with("POST /api/users/notes "));
        assert!(requests[1].contains(r#""limit":100"#));
        assert!(requests[1].contains(r#""withRenotes":false"#));
        assert!(requests[2].contains(r#""untilId":"n2""#));
    }

    #[tokio::test]
    async fn avatar_unwraps_proxy_urls() {
        let (root, _server) = serve(vec![reply(json!({"id": "u1", "username": "alice",
            "host": null,
            "avatarUrl": "https://misskey.io/proxy/avatar.webp?url=https%3A%2F%2Fmedia.misskeyusercontent.jp%2Fio%2Fabc.png&avatar=1"}))]);
        let http = http("misskey.io", root);
        let files = media(extract(
            IO,
            &http,
            Url::parse("https://misskey.io/@alice/avatar").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://media.misskeyusercontent.jp/io/abc.png"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "avatar");
        assert_eq!(k["file"]["id"], "abc");
        assert_eq!(k["date"], "[Invalid DateTime]");
    }

    #[tokio::test]
    async fn user_queues_notes() {
        let (root, server) = serve(vec![
            reply(json!({"id": "u1", "username": "alice"})),
            reply(json!([note("n1", false)])),
            reply(json!([])),
        ]);
        let http = http("misskey.io", root);
        let files = media(extract(
            IO,
            &http,
            Url::parse("https://misskey.io/@alice").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /api/users/notes "));
    }
}
