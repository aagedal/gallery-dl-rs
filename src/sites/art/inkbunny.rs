//! Inkbunny users, galleries, scraps, pools, favorites, unread and search
//! listings, follows and posts over the Inkbunny API, mirroring
//! gallery_dl/extractor/inkbunny.py. Without credentials it logs in as
//! `guest` with every rating allowed.
use crate::error;
use crate::sites::gdl::{
    auth_info, captures, date, extr, extract_iter, group, int_of, nameext_into, parse_iso,
    parse_query, partition, re,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::sync::Mutex;
use url::Url;

const ROOT: &str = "https://inkbunny.net";
const BASE: &str = r"^https?://(?:www\.)?inkbunny\.net";
const FILENAME: &str = "{submission_id} {file_id} {title}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{username!l}"];

macro_rules! routes {
    ($($ident:ident $sub:literal),*) => { $(static $ident: Route = Route::new($sub, FILENAME, DIRECTORY, "{file_id}");)* };
}
routes!(USER "user", GALLERY "gallery", SCRAPS "scraps", POOL "pool", UNREAD "unread", SEARCH "search", FOLLOWING "following", POST "post");
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{favs_username!l}", "Favorites"],
    "{file_id}",
);

pub static SITE: Site = Site {
    category: "inkbunny",
    name: "Inkbunny",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &USER, &GALLERY, &SCRAPS, &POOL, &FAVORITE, &UNREAD, &SEARCH, &FOLLOWING, &POST,
    ],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Kind (`""`, `gallery/`, `scraps/`) and user name.
    User(String, String),
    Pool(String, String),
    Favorite(String, String),
    Unread(String),
    Search(String),
    Following(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/(gallery/|scraps/)?(\w+)(?:$|[/?#])")),
        url,
    ) {
        // Upstream's `(?!s/)`.
        if !url.path().starts_with("/s/") {
            return Some(Target::User(group(&c, 1).into(), group(&c, 2).into()));
        }
    }
    let query = |text: &str| parse_query(text);
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(?:poolview_process\.php\?pool_id=(\d+)|submissionsviewall\.php\?((?:[^#]+&)?mode=pool(?:&[^#]+)?))"
        )),
        url,
    ) {
        return Some(match c.get(1) {
            Some(id) => Target::Pool(id.as_str().into(), "pool_order".into()),
            None => {
                let params = query(group(&c, 2));
                let order = params
                    .get("orderby")
                    .map_or_else(|| "pool_order".into(), crate::sites::scalar);
                Target::Pool(
                    params
                        .get("pool_id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default(),
                    order,
                )
            }
        });
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(?:userfavorites_process\.php\?favs_user_id=(\d+)|submissionsviewall\.php\?((?:[^#]+&)?mode=userfavs(?:&[^#]+)?))"
        )),
        url,
    ) {
        return Some(match c.get(1) {
            Some(id) => Target::Favorite(id.as_str().into(), String::new()),
            None => {
                let params = query(group(&c, 2));
                let order = params
                    .get("orderby")
                    .map_or_else(|| "fav_datetime".into(), crate::sites::scalar);
                Target::Favorite(
                    params
                        .get("user_id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default(),
                    order,
                )
            }
        });
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/submissionsviewall\.php\?((?:[^#]+&)?mode=unreadsubs(?:&[^#]+)?)"
        )),
        url,
    ) {
        return Some(Target::Unread(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/submissionsviewall\.php\?((?:[^#]+&)?mode=search(?:&[^#]+)?)"
        )),
        url,
    ) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(?:watchlist_process\.php\?mode=watching&user_id=(\d+)|usersviewall\.php\?((?:[^#]+&)?mode=watching(?:&[^#]+)?))"
        )),
        url,
    ) {
        let id = c.get(1).map(|m| m.as_str().to_owned()).unwrap_or_else(|| {
            query(group(&c, 2))
                .get("user_id")
                .map(crate::sites::scalar)
                .unwrap_or_default()
        });
        return Some(Target::Following(id));
    }
    captures(re!(&format!(r"{BASE}/s/(\d+)")), url).map(|c| Target::Post(group(&c, 1).into()))
}

/// The API session id (upstream caches it per user).
static SESSION: Mutex<Option<String>> = Mutex::new(None);

fn session() -> String {
    SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// `authenticate`: log in (as `guest` without credentials).
async fn authenticate(http: &Http, invalidate: bool) -> crate::Result<()> {
    if !invalidate && SESSION.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Ok(());
    }
    let (username, password) = auth_info(http).unwrap_or_else(|| ("guest".into(), String::new()));
    let data = http
        .post_form(
            &http.api(&format!("{ROOT}/api_login.php"))?,
            &[
                ("username", username.as_str()),
                ("password", password.as_str()),
            ],
            &[],
        )
        .await?;
    let sid = data["sid"]
        .as_str()
        .ok_or_else(|| {
            error(format!(
                "Inkbunny login failed: {}",
                crate::sites::scalar(&data["error_message"])
            ))
        })?
        .to_owned();
    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid);
    if username == "guest" {
        call(
            http,
            "userrating",
            &[
                ("tag[2]", "yes".into()),
                ("tag[3]", "yes".into()),
                ("tag[4]", "yes".into()),
                ("tag[5]", "yes".into()),
            ],
        )
        .await?;
    }
    Ok(())
}

/// `_call`: an API request, re-authenticating once on expired sessions.
async fn call(http: &Http, endpoint: &str, params: &[(&str, String)]) -> crate::Result<Value> {
    for attempt in 0..2 {
        let mut url = http.api(&format!("{ROOT}/api_{endpoint}.php"))?;
        url.query_pairs_mut()
            .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
            .append_pair("sid", &session());
        let data = http.json(&url).await?;
        let Some(code) = data.get("error_code") else {
            return Ok(data);
        };
        if crate::sites::scalar(code) == "2" && attempt == 0 {
            Box::pin(authenticate(http, true)).await?;
            continue;
        }
        return Err(error(format!(
            "Inkbunny API error: {}",
            crate::sites::scalar(&data["error_message"])
        )));
    }
    Err(error("Inkbunny API error"))
}

/// `detail`: full submissions for ids, in order.
async fn detail(http: &Http, ids: &[String]) -> crate::Result<Vec<Value>> {
    let data = call(
        http,
        "submissions",
        &[
            ("submission_ids", ids.join(",")),
            ("show_description", "yes".into()),
            ("show_pools", "yes".into()),
        ],
    )
    .await?;
    let subs = data["submissions"].as_array().cloned().unwrap_or_default();
    Ok(ids
        .iter()
        .filter_map(|id| {
            subs.iter()
                .find(|s| crate::sites::scalar(&s["submission_id"]) == *id)
                .cloned()
        })
        .collect())
}

/// `InkbunnyExtractor.items` for one submission.
fn post_items(post: &Value, extra: &Map<String, Value>) -> Vec<Item> {
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    post.extend(extra.clone());
    let created = post
        .get("create_datetime")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    post.insert(
        "date".into(),
        date(parse_iso(created.get(..19).unwrap_or(&created))),
    );
    let tags: Vec<Value> = post
        .get("keywords")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|k| k["keyword_name"].clone())
        .collect();
    post.insert("tags".into(), tags.into());
    let ratings: Vec<Value> = post
        .get("ratings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| r["name"].clone())
        .collect();
    post.insert("ratings".into(), ratings.into());
    let files = post
        .remove("files")
        .and_then(|f| f.as_array().cloned())
        .unwrap_or_default();
    post.remove("keywords");
    for key in [
        "deleted",
        "favorite",
        "friends_only",
        "guest_block",
        "hidden",
        "public",
        "scraps",
    ] {
        if let Some(value) = post.get(key) {
            let flag = value.as_str() == Some("t");
            post.insert(key.into(), flag.into());
        }
    }
    let sid = session();
    let submission = crate::sites::scalar(&post["submission_id"]);
    files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            post.insert("num".into(), (index + 1).into());
            if let Some(file) = file.as_object() {
                post.extend(file.clone());
            }
            post.insert(
                "deleted".into(),
                (file["deleted"].as_str() == Some("t")).into(),
            );
            let created = file["create_datetime"].as_str().unwrap_or_default();
            post.insert(
                "date".into(),
                date(parse_iso(created.get(..19).unwrap_or(created))),
            );
            nameext_into(file["file_name"].as_str().unwrap_or_default(), &mut post);
            let mut url = file["file_url_full"].as_str()?.to_owned();
            if url.contains("/private_files/") {
                url = format!("{url}?sid={sid}");
            }
            let file_id = crate::sites::scalar(&file["file_id"]);
            Some(Item::new(Url::parse(&url).ok()?, post.clone()).ids(submission.clone(), file_id))
        })
        .collect()
}

/// `_pagination_search`: submission ids page by page, then their details.
fn search<'a>(
    http: &'a Http,
    route: &'static Route,
    params: Vec<(String, String)>,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some((1i64, None::<String>)), move |state| {
        let params = params.clone();
        let extra = extra.clone();
        async move {
            let Some((page, rid)) = state else {
                return Ok(None);
            };
            if page == 1 {
                authenticate(http, false).await?;
            }
            let mut query: Vec<(&str, String)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect();
            query.push(("page", page.to_string()));
            query.push(("submission_ids_only", "yes".into()));
            match &rid {
                Some(rid) => query.push(("rid", rid.clone())),
                None => query.push(("get_rid", "yes".into())),
            }
            let data = call(http, "search", &query).await?;
            let ids: Vec<String> = data["submissions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| crate::sites::scalar(&s["submission_id"]))
                .collect();
            if ids.is_empty() {
                return Ok(None);
            }
            let posts = detail(http, &ids).await?;
            let events = posts
                .iter()
                .flat_map(|post| crate::sites::post(&SITE, http, route, post_items(post, &extra)))
                .collect();
            let next = (int_of(data.get("page")) < int_of(data.get("pages_count"))).then(|| {
                (
                    page + 1,
                    rid.or_else(|| data.get("rid").map(crate::sites::scalar)),
                )
            });
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn params(pairs: &[(&str, Option<String>)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .filter_map(|(k, v)| Some(((*k).to_owned(), v.clone()?)))
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Inkbunny URL"));
    };
    let orderby = |default: Option<&str>| {
        http.option("orderby")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(default.map(str::to_owned))
    };
    match target {
        Target::User(kind, user) => {
            let (route, scraps): (&'static Route, Option<&str>) = match kind.as_str() {
                "gallery/" => (&GALLERY, Some("no")),
                "scraps/" => (&SCRAPS, Some("only")),
                _ => (&USER, None),
            };
            let order = orderby(None);
            let unread = order
                .as_deref()
                .is_some_and(|o| o.starts_with("unread_"))
                .then(|| "yes".to_owned());
            search(
                http,
                route,
                params(&[
                    ("username", Some(user)),
                    ("scraps", scraps.map(str::to_owned)),
                    ("orderby", order),
                    ("unread_submissions", unread),
                ]),
                Map::new(),
            )
        }
        Target::Pool(id, order) => {
            let mut extra = Map::new();
            extra.insert("pool_id".into(), id.as_str().into());
            search(
                http,
                &POOL,
                params(&[("pool_id", Some(id)), ("orderby", Some(order))]),
                extra,
            )
        }
        Target::Favorite(user_id, order) => crate::sites::once(async move {
            let order = if order.is_empty() {
                orderby(Some("fav_datetime"))
            } else {
                Some(order)
            };
            let page = http
                .text(&http.api(&format!(
                    "{ROOT}/userfavorites_process.php?favs_user_id={user_id}"
                ))?)
                .await?;
            let name = extr(extr(&page, "<a rel=\"author\"", "</a>"), "href=\"/", "\"").to_owned();
            let mut extra = Map::new();
            extra.insert("favs_user_id".into(), user_id.as_str().into());
            extra.insert("favs_username".into(), name.into());
            let unread = order
                .as_deref()
                .is_some_and(|o| o.starts_with("unread_"))
                .then(|| "yes".to_owned());
            Ok(search(
                http,
                &FAVORITE,
                params(&[
                    ("favs_user_id", Some(user_id)),
                    ("orderby", order),
                    ("unread_submissions", unread),
                ]),
                extra,
            ))
        }),
        Target::Unread(query) => {
            let mut params: Vec<(String, String)> = parse_query(&query)
                .into_iter()
                .filter(|(k, _)| k != "rid" && k != "mode")
                .map(|(k, v)| (k, crate::sites::scalar(&v)))
                .collect();
            params.push(("unread_submissions".into(), "yes".into()));
            search(http, &UNREAD, params, Map::new())
        }
        Target::Search(query) => crate::sites::once(async move {
            let mut map = parse_query(&query);
            let mut extra = Map::new();
            extra.insert("search".into(), Value::Object(map.clone()));
            map.remove("rid");
            let mut take = |key: &str| map.remove(key).map(|v| crate::sites::scalar(&v));
            let joined = take("stringtype");
            let days = take("days");
            let artist = take("artist");
            let favsby = take("favsby");
            let mut params: Vec<(String, String)> = map
                .iter()
                .map(|(k, v)| (k.clone(), crate::sites::scalar(v)))
                .collect();
            params.extend(self::params(&[
                ("string_join_type", joined),
                ("dayslimit", days),
                ("username", artist),
            ]));
            if let Some(favsby) = favsby.filter(|f| !f.is_empty()) {
                let page = http.text(&http.api(&format!("{ROOT}/{favsby}"))?).await?;
                params.push((
                    "favs_user_id".into(),
                    partition(extr(&page, "?user_id=", "'"), "&").0.to_owned(),
                ));
            }
            Ok(search(http, &SEARCH, params, extra))
        }),
        Target::Following(user_id) => crate::sites::once(async move {
            let request = http.api(&format!(
                "{ROOT}/watchlist_process.php?mode=watching&user_id={user_id}"
            ))?;
            let (first, final_url) = http.text_final(&request, &[]).await?;
            let mut base = final_url.clone();
            base.set_query(None);
            let mut query = parse_query(final_url.query().unwrap_or_default());
            let start = int_of(query.get("page")).max(1);
            query.remove("page");
            Ok(crate::sites::paginate(
                Some((start, Some(first))),
                move |state| {
                    let base = base.clone();
                    let query = query.clone();
                    async move {
                        let Some((page_num, page)) = state else {
                            return Ok(None);
                        };
                        let page = match page {
                            Some(page) => page,
                            None => {
                                let mut url = base.clone();
                                {
                                    let mut pairs = url.query_pairs_mut();
                                    for (k, v) in &query {
                                        pairs.append_pair(k, &crate::sites::scalar(v));
                                    }
                                    pairs.append_pair("page", &page_num.to_string());
                                }
                                http.text(&url).await?
                            }
                        };
                        let start = page.find("id=\"changethumboriginal_form\"").unwrap_or(0);
                        let children = extract_iter(
                            &page[start..],
                            "<a class=\"widget_userNameSmall\" href=\"",
                            "\"",
                        )
                        .into_iter()
                        .filter_map(|user| {
                            Some((Url::parse(&format!("{ROOT}{user}")).ok()?, Map::new()))
                        })
                        .collect();
                        let next = page
                            .contains("<a title='next page' ")
                            .then_some((page_num + 1, None));
                        Ok(Some((
                            crate::sites::galleries::queue(http, &SITE, &FOLLOWING, children),
                            next,
                        )))
                    }
                },
            ))
        }),
        Target::Post(id) => crate::sites::once(async move {
            authenticate(http, false).await?;
            let posts = detail(http, std::slice::from_ref(&id)).await?;
            let post = posts
                .first()
                .ok_or_else(|| error("Inkbunny submission not found"))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                post_items(post, &Map::new()),
            )))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://inkbunny.net/soina"),
            Some(Target::User(String::new(), "soina".into()))
        );
        assert_eq!(
            t("https://inkbunny.net/scraps/soina/"),
            Some(Target::User("scraps/".into(), "soina".into()))
        );
        assert_eq!(
            t("https://inkbunny.net/poolview_process.php?pool_id=28985"),
            Some(Target::Pool("28985".into(), "pool_order".into()))
        );
        assert_eq!(
            t(
                "https://inkbunny.net/submissionsviewall.php?rid=ffffffffff&mode=userfavs&random=no&orderby=fav_datetime&page=1&user_id=20969"
            ),
            Some(Target::Favorite("20969".into(), "fav_datetime".into()))
        );
        assert_eq!(
            t("https://inkbunny.net/s/1829715"),
            Some(Target::Post("1829715".into()))
        );
    }

    #[tokio::test]
    async fn post_details_files() {
        let post = json!({"submissions": [{"submission_id": "1829715", "title": "Kitty", "username": "Soina", "create_datetime": "2019-03-21 23:00:44.47-05",
            "keywords": [{"keyword_name": "cat"}], "ratings": [{"name": "General"}], "public": "t",
            "files": [{"file_id": "2626062", "file_name": "1553223643.soina_kitty.png", "create_datetime": "2019-03-21 23:00:44.5-05", "deleted": "f",
                "file_url_full": "https://us.ib.metapix.net/files/full/2626/2626062_soina_kitty.png"}]}]});
        let (root, server) = serve(vec![
            json(json!({"sid": "S"})),
            json(json!({"sid": "S"})),
            json(post),
        ]);
        let http = client("inkbunny", root);
        *SESSION.lock().unwrap() = None;
        let files = media(extract(
            &http,
            Url::parse("https://inkbunny.net/s/1829715").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["public"].as_bool(),
                k["deleted"].as_bool(),
                k["tags"][0].as_str()
            ),
            (Some(true), Some(false), Some("cat"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1829715 2626062 Kitty.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("inkbunny/soina")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api_login.php "));
        assert!(requests[1].starts_with("GET /api_userrating.php?tag%5B2%5D=yes"));
    }
}
