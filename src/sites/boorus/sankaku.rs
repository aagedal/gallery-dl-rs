//! Sankaku Channel tag searches, pools/books, posts and book searches,
//! mirroring gallery_dl/extractor/sankaku.py through `sankakuapi.com`.
//! Options: `username`/`password`, `tags` (`true` or `extended`), `notes`,
//! `url`.
use crate::error;
use crate::sites::boorus::{booru_fields, per_post};
use crate::sites::gdl::{
    auth_info, captures, date, group, parse_query, parse_ts, re, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use url::Url;

const API: &str = "https://sankakuapi.com";
const ROOT: &str = "https://sankaku.app";
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";
const BASE: &str = r"^https?://(?:(?:chan|www|beta|black|white)\.sankakucomplex\.com|sankaku\.app)(?:/[a-z]{2}(?:[-_][A-Z]{2})?)?";
const PER_PAGE: usize = 100;

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POOL: Route = Route::new(
    "pool",
    FILENAME,
    &["{category}", "pool", "{pool[id]} {pool[name_en]}"],
    "p_{pool}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");
static BOOKS: Route = Route::new("books", FILENAME, &["{category}"], "");

pub static SITE: Site = Site {
    category: "sankaku",
    name: "Sankaku Channel",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &POOL, &POST, &BOOKS],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Tag(String),
    Pool(String),
    Post(String),
    Books(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}(?:/posts)?/?\?([^#]*)")), url) {
        return Some(Target::Tag(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/(?:books|pools?/show)/(\w+)")), url) {
        return Some(Target::Pool(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/posts?(?:/show)?/(\w+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/books/?\?([^#]*)")), url)
        .map(|c| Target::Books(group(&c, 1).into()))
}

/// The `tags` query value, with `date:` tags rewritten to ISO form.
fn search_tags(query: &str) -> String {
    let tags = parse_query(query)
        .get("tags")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let tags = unquote(&tags.replace('+', " "));
    if !tags.contains("date:") {
        return tags;
    }
    // `date:DD.MM.YYYY` / `date:YYYY-MM-DD` not followed by `T`.
    let rewrite = |tags: &str, pattern: &regex::Regex, order: [usize; 3]| -> String {
        let mut out = String::new();
        let mut last = 0;
        for c in pattern.captures_iter(tags) {
            let m = c.get(0).unwrap();
            if tags[m.end()..].starts_with('T') {
                continue;
            }
            out.push_str(&tags[last..m.start()]);
            out.push_str(&format!(
                "date:{}-{}-{}T00:00",
                &c[order[0]], &c[order[1]], &c[order[2]]
            ));
            last = m.end();
        }
        out.push_str(&tags[last..]);
        out
    };
    let tags = rewrite(
        &tags,
        re!(r"date:(\d\d)[.-](\d\d)[.-](\d\d\d\d)"),
        [3, 2, 1],
    );
    rewrite(
        &tags,
        re!(r"date:(\d\d\d\d)[.-](\d\d)[.-](\d\d)"),
        [1, 2, 3],
    )
}

/// Bearer tokens by username for this process (upstream caches a year).
fn tokens() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static TOKENS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, String>>,
    > = std::sync::OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

#[derive(Clone)]
struct Api<'a> {
    http: &'a Http,
    auth: Option<(String, String)>,
}

impl<'a> Api<'a> {
    fn headers(&self, token: Option<&str>) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("Accept", "application/vnd.sankaku.api+json;v=2".to_owned()),
            ("Origin", ROOT.to_owned()),
        ];
        if let Some(token) = token {
            headers.push(("Authorization", token.to_owned()));
        }
        headers
    }

    async fn token(&self, refresh: bool) -> crate::Result<Option<String>> {
        let Some((username, password)) = &self.auth else {
            return Ok(None);
        };
        if !refresh
            && let Some(token) = tokens()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(username)
        {
            return Ok(Some(token.clone()));
        }
        let headers = self.headers(None);
        let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let data = self
            .http
            .post_json(
                &self.http.api(&format!("{API}/auth/token"))?,
                &json!({"login": username, "password": password}),
                &refs,
            )
            .await
            .map_err(|_| error("Sankaku login failed; check the username and password"))?;
        if !crate::sites::gdl::truthy(data.get("success")) {
            return Err(error(format!(
                "Sankaku login failed: {}",
                crate::sites::scalar(&data["error"])
            )));
        }
        let token = format!("Bearer {}", crate::sites::scalar(&data["access_token"]));
        tokens()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(username.clone(), token.clone());
        Ok(Some(token))
    }

    /// `_call`: retries once with a fresh token on authorization errors.
    async fn call(&self, endpoint: &str, params: &[(String, String)]) -> crate::Result<Value> {
        let mut refresh = false;
        loop {
            let token = self.token(refresh).await?;
            let mut url = self.http.api(&format!("{API}{endpoint}"))?;
            if !params.is_empty() {
                url.query_pairs_mut().extend_pairs(params.iter());
            }
            let headers = self.headers(token.as_deref());
            let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let data = self.http.json_with(&url, &refs).await?;
            let success = data.as_object().is_none_or(|map| {
                map.get("success")
                    .is_none_or(|s| crate::sites::gdl::truthy(Some(s)))
            });
            if success {
                return Ok(data);
            }
            let code = crate::sites::gdl::str_of(&data, "code").to_owned();
            if !refresh
                && self.auth.is_some()
                && ["unauthorized", "invalid-token", "invalid_token"]
                    .iter()
                    .any(|s| code.ends_with(s))
            {
                refresh = true;
                continue;
            }
            let reason = crate::sites::gdl::rpartition(&code, "__")
                .2
                .replace(['-', '_'], " ");
            return Err(error(format!("Sankaku API error '{reason}'")));
        }
    }

    /// `_pagination` over keyset endpoints (`meta.next` cursors).
    fn keyset(
        self,
        endpoint: &'static str,
        mut params: Vec<(String, String)>,
    ) -> futures_util::stream::BoxStream<'a, crate::Result<Vec<Value>>> {
        params.push(("lang".into(), "en".into()));
        params.push(("limit".into(), PER_PAGE.to_string()));
        futures_util::stream::try_unfold((self, Some(params)), move |(api, state)| async move {
            let Some(mut params) = state else {
                return Ok(None);
            };
            let data = api.call(endpoint, &params).await?;
            let posts = data["data"].as_array().cloned().unwrap_or_default();
            let next = data["meta"]["next"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(|next| {
                    params.retain(|(k, _)| k != "next");
                    params.push(("next".into(), next.to_owned()));
                    params
                });
            Ok(Some((posts, (api, next))))
        })
        .boxed()
    }
}

const TAG_TYPES: [&str; 10] = [
    "general",
    "artist",
    "studio",
    "copyright",
    "character",
    "genre",
    "",
    "",
    "medium",
    "meta",
];

/// `BooruExtractor.items` for one post: file URL, tags/notes, `_prepare`.
async fn post_item(
    api: &Api<'_>,
    post: Value,
    extra: &Map<String, Value>,
) -> crate::Result<Option<Item>> {
    let http = api.http;
    let Some(mut post) = post.as_object().cloned() else {
        return Ok(None);
    };
    let key = http.option_str("url").unwrap_or("file_url").to_owned();
    let mut raw = post
        .get(&key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.as_bytes().get(4) != Some(&b's') {
        raw = format!("https{}", &raw[4.min(raw.len())..]);
    }
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    match http.option("tags") {
        Some(Value::String(mode)) if mode == "extended" => {
            let url = format!("https://chan.sankakucomplex.com/posts/{id}");
            if let Ok(page) = http.text_with(&http.api(&url)?, &[("Referer", &url)]).await {
                let sidebar = crate::sites::gdl::extr(&page, "<ul id=\"tag-sidebar\"", "</ul>");
                let mut groups: Vec<(String, Vec<String>)> = Vec::new();
                for c in re!(r#"tag-type-([^"' ]+).*?\?tags=([^"'&]+)"#).captures_iter(sidebar) {
                    let name = unescape(&unquote(&c[2]));
                    match groups.iter_mut().find(|(t, _)| t == &c[1]) {
                        Some((_, list)) => list.push(name),
                        None => groups.push((c[1].to_owned(), vec![name])),
                    }
                }
                for (kind, values) in groups {
                    post.insert(format!("tag_string_{kind}"), values.join(" ").into());
                    post.insert(format!("tags_{kind}"), values.into());
                }
            }
        }
        Some(value) if crate::sites::gdl::truthy(Some(value)) => {
            let mut groups: Vec<(i64, Vec<String>)> = Vec::new();
            let mut page = 1;
            loop {
                let params = [
                    ("lang".to_owned(), "en".to_owned()),
                    ("page".into(), page.to_string()),
                    ("limit".into(), "100".into()),
                ];
                let data = api.call(&format!("/posts/{id}/tags"), &params).await?;
                let batch = data["data"].as_array().cloned().unwrap_or_default();
                for tag in &batch {
                    let Some(name) = tag["name"].as_str().filter(|n| !n.is_empty()) else {
                        continue;
                    };
                    let kind = tag["type"].as_i64().unwrap_or(0);
                    let name = name.to_lowercase().replace(' ', "_");
                    match groups.iter_mut().find(|(t, _)| *t == kind) {
                        Some((_, list)) => list.push(name),
                        None => groups.push((kind, vec![name])),
                    }
                }
                let total: usize = groups.iter().map(|(_, v)| v.len()).sum();
                if batch.len() < 80 || total as i64 >= crate::sites::gdl::int_of(data.get("total"))
                {
                    break;
                }
                page += 1;
            }
            for (kind, values) in groups {
                let name = TAG_TYPES.get(kind as usize).copied().unwrap_or_default();
                post.insert(format!("tag_string_{name}"), values.join(" ").into());
                post.insert(format!("tags_{name}"), values.into());
            }
        }
        _ => {}
    }
    if http.option_bool("notes", false) {
        let notes = if crate::sites::gdl::truthy(post.get("has_notes")) {
            let mut notes = api
                .call(
                    &format!("/posts/{id}/notes"),
                    &[("lang".into(), "en".into())],
                )
                .await?;
            for note in notes.as_array_mut().into_iter().flatten() {
                for key in ["created_at", "updated_at"] {
                    let value = note[key]["s"].clone();
                    note[key] = value;
                }
            }
            notes
        } else {
            Value::Array(Vec::new())
        };
        post.insert("notes".into(), notes);
    }
    let Some(url) = Url::parse(&raw).ok().filter(crate::sites::plain) else {
        return Ok(None);
    };
    booru_fields(&url, &mut post, extra);
    let created = post
        .get("created_at")
        .map(|c| c["s"].clone())
        .unwrap_or(Value::Null);
    post.insert("date".into(), date(parse_ts(&created)));
    post.insert("created_at".into(), created);
    let tags: Vec<Value> = post
        .remove("tag_names")
        .and_then(|t| t.as_array().cloned())
        .unwrap_or_default();
    post.insert(
        "tag_string".into(),
        tags.iter()
            .map(crate::sites::scalar)
            .collect::<Vec<_>>()
            .join(" ")
            .into(),
    );
    post.insert("tags".into(), tags.into());
    Ok(Some(Item::new(url, post).ids(id.clone(), id)))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Sankaku URL"));
    };
    let api = Api {
        http,
        auth: auth_info(http),
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let tag = if id.len() == 32 {
                format!("md5:{id}")
            } else {
                format!("id_range:{id}")
            };
            let params = [
                ("lang".to_owned(), "en".to_owned()),
                ("page".into(), "1".into()),
                ("limit".into(), "1".into()),
                ("tags".into(), tag),
            ];
            let posts = api.call("/v2/posts", &params).await?;
            let mut items = Vec::new();
            for post in posts.as_array().cloned().unwrap_or_default() {
                items.extend(post_item(&api, post, &Map::new()).await?);
            }
            Ok(crate::sites::events(per_post(&SITE, http, &POST, items)))
        }),
        Target::Pool(id) => crate::sites::once(async move {
            let mut pool = api
                .call(&format!("/pools/{id}"), &[("lang".into(), "en".into())])
                .await?;
            let posts = pool
                .as_object_mut()
                .and_then(|p| p.remove("posts"))
                .and_then(|p| p.as_array().cloned())
                .unwrap_or_default();
            for key in ["tags", "artist_tags"] {
                let names: Vec<Value> = pool[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| t["name"].clone())
                    .collect();
                pool[key] = names.into();
            }
            let mut extra = Map::new();
            extra.insert("pool".into(), pool);
            let mut items = Vec::new();
            for (index, mut post) in posts.into_iter().enumerate() {
                post["num"] = (index + 1).into();
                items.extend(post_item(&api, post, &extra).await?);
            }
            Ok(crate::sites::events(per_post(&SITE, http, &POOL, items)))
        }),
        Target::Tag(query) => {
            let tags = search_tags(&query);
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags.as_str().into());
            let mut sources = Vec::new();
            if tags.contains("parent:") {
                let parent = re!(r"\bparent:(\w+)")
                    .replace_all(&tags, "id_range:$1")
                    .into_owned();
                sources.push(parent);
            }
            sources.push(tags);
            let pages_api = api.clone();
            futures_util::stream::iter(sources)
                .map(move |tags| {
                    pages_api
                        .clone()
                        .keyset("/v2/posts/keyset", vec![("tags".into(), tags)])
                })
                .flatten()
                .then(move |page| {
                    let extra = extra.clone();
                    let api = api.clone();
                    async move {
                        let mut items = Vec::new();
                        for post in page? {
                            items.extend(post_item(&api, post, &extra).await?);
                        }
                        Ok::<_, crate::Error>(crate::sites::events(per_post(
                            &SITE, http, &TAG, items,
                        )))
                    }
                })
                .map(|result| result.unwrap_or_else(crate::sites::fail))
                .flatten()
                .boxed()
        }
        Target::Books(query) => {
            let tags = unquote(
                &parse_query(&query)
                    .get("tags")
                    .map(crate::sites::scalar)
                    .unwrap_or_default()
                    .replace('+', " "),
            );
            api.keyset(
                "/pools/keyset",
                vec![("tags".into(), tags), ("pool_type".into(), "0".into())],
            )
            .map(move |page| match page {
                Ok(pools) => {
                    let children: Vec<(Url, Map<String, Value>)> = pools
                        .into_iter()
                        .filter_map(|pool| {
                            Some((
                                Url::parse(&format!(
                                    "{ROOT}/books/{}",
                                    crate::sites::scalar(&pool["id"])
                                ))
                                .ok()?,
                                pool.as_object()?.clone(),
                            ))
                        })
                        .collect();
                    futures_util::stream::iter(children)
                        .map(move |(url, data)| {
                            crate::sites::queue(http, SITE.category, BOOKS.subcategory, url, &data)
                        })
                        .flatten()
                        .boxed()
                }
                Err(e) => crate::sites::fail(e),
            })
            .flatten()
            .boxed()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json as reply, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://sankaku.app/zh-CN/?tags=order%3Apopularity"),
            Some(Target::Tag("tags=order%3Apopularity".into()))
        );
        assert_eq!(
            t("https://chan.sankakucomplex.com/ja/posts/?tags=x"),
            Some(Target::Tag("tags=x".into()))
        );
        assert_eq!(
            t("https://chan.sankakucomplex.com/pools/show/90"),
            Some(Target::Pool("90".into()))
        );
        assert_eq!(
            t("https://sankaku.app/post/show/y0abGlDOr2o"),
            Some(Target::Post("y0abGlDOr2o".into()))
        );
        assert_eq!(
            t("https://sankaku.app/books?tags=aiue"),
            Some(Target::Books("tags=aiue".into()))
        );
        assert_eq!(t("https://example.com/?tags=x"), None);
    }

    #[test]
    fn rewrites_date_tags() {
        assert_eq!(
            search_tags("tags=date:20.03.2023+a"),
            "date:2023-03-20T00:00 a"
        );
        assert_eq!(search_tags("tags=date:2023-03-20"), "date:2023-03-20T00:00");
        assert_eq!(
            search_tags("tags=date:2023-03-20T00:00"),
            "date:2023-03-20T00:00"
        );
    }

    fn post(id: &str) -> Value {
        json!({"id": id, "md5": "abc", "file_url": "http://v.sankakucomplex.com/data/ab/abc.jpg?e=1",
            "created_at": {"s": 1600000000}, "tag_names": ["bonocho", "1girl"], "has_notes": false})
    }

    #[tokio::test]
    async fn pages_tag_search() {
        let (root, server) = serve(vec![
            reply(json!({"data": [post("a1")], "meta": {"next": "N2"}})),
            reply(json!({"data": [post("a2")], "meta": {"next": null}})),
        ]);
        let http = client("sankaku", root);
        let files = media(extract(
            &http,
            Url::parse("https://sankaku.app/?tags=bonocho").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://v.sankakucomplex.com/data/ab/abc.jpg?e=1"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tag_string"], "bonocho 1girl");
        assert_eq!(k["date"], "2020-09-13 12:26:40");
        assert_eq!(k["search_tags"], "bonocho");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "sankaku_a1_abc.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("sankaku/bonocho")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v2/posts/keyset?tags=bonocho&lang=en&limit=100 "));
        assert!(requests[1].contains("&next=N2 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("accept: application/vnd.sankaku.api+json;v=2")
        );
    }

    #[tokio::test]
    async fn logs_in_and_reads_tag_types() {
        let (root, server) = serve(vec![
            reply(json!({"success": true, "access_token": "TOKEN"})),
            reply(json!([post("p1")])),
            reply(
                json!({"total": 2, "data": [{"name": "Bono Cho", "type": 1}, {"name": "solo", "type": 0}]}),
            ),
        ]);
        let options = json!({"username": "u", "password": "p", "tags": true});
        let http = client("sankaku", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://sankaku.app/posts/p1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["tags_artist"], json!(["bono_cho"]));
        assert_eq!(k["tag_string_general"], "solo");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /auth/token "));
        assert!(
            requests[1].starts_with("GET /v2/posts?lang=en&page=1&limit=1&tags=id_range%3Ap1 ")
        );
        assert!(requests[1].contains("Bearer TOKEN"));
    }

    #[tokio::test]
    async fn books_queue_pools() {
        let (root, server) = serve(vec![reply(
            json!({"data": [{"id": "90"}, {"id": "91"}], "meta": {"next": null}}),
        )]);
        let http = client("sankaku", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://sankaku.app/books?tags=aiue").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
