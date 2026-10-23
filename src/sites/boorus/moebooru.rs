//! Moebooru sites (yande.re, Konachan, Sakugabooru, Lolibooru), mirroring
//! gallery_dl/extractor/moebooru.py through the `post.json` API.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const PER_PAGE: usize = 100;
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POOL: Route = Route::new(
    "pool",
    FILENAME,
    &["{category}", "pool", "{pool}"],
    "p_{pool}_{id}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");
static POPULAR: Route = Route::new(
    "popular",
    FILENAME,
    &["{category}", "popular", "{scale}", "{date}"],
    "P_{scale[0]}_{date}_{id}",
);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&POST, &TAG, &POOL, &POPULAR],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(YANDERE, "yandere", "yande.re");
instance!(KONACHAN, "konachan", "Konachan");
instance!(SAKUGABOORU, "sakugabooru", "Sakugabooru");
instance!(LOLIBOORU, "lolibooru", "Lolibooru");

fn site(category: &str) -> &'static Site {
    match category {
        "yandere" => &YANDERE,
        "konachan" => &KONACHAN,
        "sakugabooru" => &SAKUGABOORU,
        _ => &LOLIBOORU,
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "yandere" => "https://yande.re",
        "konachan" => "https://konachan.com",
        "sakugabooru" => "https://www.sakugabooru.com",
        _ => "https://lolibooru.moe",
    }
}

fn hosts(category: &str) -> &'static [&'static str] {
    match category {
        "yandere" => &["yande.re"],
        "konachan" => &["konachan.com", "konachan.net"],
        "sakugabooru" => &["sakugabooru.com"],
        _ => &["lolibooru.moe"],
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    Tag(String),
    Pool(String),
    Post(String),
    Popular {
        scale: String,
        query: Vec<(String, String)>,
    },
}

fn digits(text: &str) -> bool {
    (1..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let host = url.host_str()?;
    if !crate::sites::plain(url)
        || !hosts(category)
            .iter()
            .any(|h| host == *h || (category != "yandere" && host == format!("www.{h}")))
    {
        return None;
    }
    let parts = crate::sites::segments(url);
    match parts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["post"] => url
            .query_pairs()
            .find(|(k, _)| k == "tags")
            .map(|(_, v)| Target::Tag(v.into_owned())),
        ["pool", "show", id, ..] if digits(id) => Some(Target::Pool((*id).into())),
        ["post", "show", id, ..] if digits(id) => Some(Target::Post((*id).into())),
        ["post", view] => {
            let scale = view.strip_prefix("popular_")?;
            matches!(scale, "by_day" | "by_week" | "by_month" | "recent").then(|| Target::Popular {
                scale: scale.into(),
                query: url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
            })
        }
        _ => None,
    }
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error("unsupported Moebooru URL"));
    };
    let base = root(category);
    match target {
        Target::Tag(tags) => {
            let mut data = Map::new();
            data.insert("search_tags".into(), tags.clone().into());
            paginate(http, category, &TAG, tags, data)
        }
        Target::Pool(id) => {
            let mut data = Map::new();
            data.insert(
                "pool".into(),
                id.parse::<u64>().map_or(Value::Null, Value::from),
            );
            paginate(http, category, &POOL, format!("pool:{id}"), data)
        }
        Target::Post(id) => crate::sites::once(async move {
            let url = http.api(&format!("{base}/post.json?tags=id:{id}"))?;
            let posts = http.json(&url).await?;
            Ok(crate::sites::events(posts_events(
                category,
                http,
                &POST,
                posts,
                &Map::new(),
            )))
        }),
        Target::Popular { scale, query } => crate::sites::once(async move {
            let data = popular_metadata(&scale, &query, today())?;
            let mut url = http.api(&format!("{base}/post/popular_{scale}.json"))?;
            if !query.is_empty() {
                url.query_pairs_mut().extend_pairs(&query);
            }
            let posts = http.json(&url).await?;
            Ok(crate::sites::events(posts_events(
                category, http, &POPULAR, posts, &data,
            )))
        }),
    }
}

fn today() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

/// Popular view `date`/`scale` keywords; weekly views start on Monday.
fn popular_metadata(
    scale: &str,
    query: &[(String, String)],
    today: time::Date,
) -> Result<Map<String, Value>> {
    let get = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let mut date = match get("year") {
        Some(year) => format!(
            "{:0>4}-{:0>2}-{:0>2}",
            year,
            get("month").unwrap_or("01"),
            get("day").unwrap_or("01")
        ),
        None => format!(
            "{:04}-{:02}-{:02}",
            today.year(),
            u8::from(today.month()),
            today.day()
        ),
    };
    let scale = scale.strip_prefix("by_").unwrap_or(scale);
    if scale == "week" {
        let format = time::macros::format_description!("[year]-[month]-[day]");
        let day = time::Date::parse(&date, &format)
            .map_err(|_| error("invalid Moebooru popular date"))?;
        let monday = day - time::Duration::days(day.weekday().number_days_from_monday().into());
        date = format!(
            "{:04}-{:02}-{:02}",
            monday.year(),
            u8::from(monday.month()),
            monday.day()
        );
    } else if scale == "month" {
        date.truncate(date.len().saturating_sub(3));
    }
    let mut data = Map::new();
    data.insert("date".into(), date.into());
    data.insert("scale".into(), scale.into());
    Ok(data)
}

fn paginate<'a>(
    http: &'a Http,
    category: &'static str,
    route: &'static Route,
    tags: String,
    data: Map<String, Value>,
) -> Events<'a> {
    let base = root(category);
    crate::sites::paginate(Some(1u64), move |page| {
        let tags = tags.clone();
        let data = data.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{base}/post.json"))?;
            url.query_pairs_mut()
                .append_pair("tags", &tags)
                .append_pair("page", &page.to_string())
                .append_pair("limit", &PER_PAGE.to_string());
            let posts = http.json(&url).await?;
            let count = posts.as_array().map_or(0, Vec::len);
            let events = posts_events(category, http, route, posts, &data);
            let next = (count >= PER_PAGE && page < 10_000).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// One post boundary per API post, as upstream yields a Directory per post.
fn posts_events(
    category: &str,
    http: &Http,
    route: &'static Route,
    posts: Value,
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let site = site(category);
    let mut events = Vec::new();
    for post in posts.as_array().into_iter().flatten() {
        if let Some(item) = parse_post(category, post, data) {
            events.extend(crate::sites::post(site, http, route, vec![item]));
        }
    }
    events
}

pub(crate) fn parse_post(category: &str, post: &Value, data: &Map<String, Value>) -> Option<Item> {
    let mut fields = post.as_object()?.clone();
    let raw = fields.get("file_url")?.as_str()?;
    let raw = if raw.starts_with('/') {
        format!("{}{raw}", root(category))
    } else {
        raw.to_owned()
    };
    let url = Url::parse(&raw).ok().filter(crate::sites::plain)?;
    let id = fields.get("id").map(crate::sites::scalar)?;
    if !fields.contains_key("extension") {
        let (name, ext) = crate::sites::nameext(&url);
        fields.insert("filename".into(), name.into());
        fields.insert("extension".into(), ext.into());
    }
    for (key, value) in data {
        fields.insert(key.clone(), value.clone());
    }
    let created = fields.get("created_at").and_then(Value::as_i64);
    fields.insert(
        "date".into(),
        crate::filter::source_date_text(created).into(),
    );
    let user = fields
        .get("author")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let tags = fields
        .get("tags")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(
        Item::new(url, fields)
            .ids(id.clone(), id)
            .user(user)
            .caption(tags)
            .timestamp(created),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("yandere", "https://yande.re/post?tags=landscape+sky"),
            Some(Target::Tag("landscape sky".into()))
        );
        assert_eq!(
            t("konachan", "https://konachan.net/pool/show/95"),
            Some(Target::Pool("95".into()))
        );
        assert_eq!(
            t(
                "sakugabooru",
                "https://www.sakugabooru.com/post/show/125570"
            ),
            Some(Target::Post("125570".into()))
        );
        assert!(matches!(
            t(
                "yandere",
                "https://yande.re/post/popular_by_week?day=20&month=8&year=2019"
            ),
            Some(Target::Popular { .. })
        ));
        assert_eq!(t("yandere", "https://konachan.com/post/show/1"), None);
        assert_eq!(t("yandere", "https://yande.re/post/popular_by_year"), None);
        assert!(
            crate::sites::find(&Url::parse("https://konachan.com/post/show/1").unwrap())
                .is_some_and(|site| site.category == "konachan")
        );
    }

    #[test]
    fn popular_dates_match_upstream() {
        let q = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect::<Vec<_>>()
        };
        let today = time::macros::date!(2026 - 10 - 22);
        let week = popular_metadata(
            "by_week",
            &q(&[("year", "2019"), ("month", "8"), ("day", "22")]),
            today,
        )
        .unwrap();
        assert_eq!(week["date"], "2019-08-19");
        assert_eq!(week["scale"], "week");
        let month =
            popular_metadata("by_month", &q(&[("year", "2019"), ("month", "8")]), today).unwrap();
        assert_eq!(month["date"], "2019-08");
        let recent = popular_metadata("recent", &[], today).unwrap();
        assert_eq!(recent["date"], "2026-10-22");
    }

    fn post(id: u64) -> Value {
        json!({"id": id, "md5": format!("md5{id}"), "created_at": 1500000000,
            "file_url": format!("https://files.yande.re/image/x/yande.re%20{id}%20tag.png"),
            "author": "uploader", "tags": "a b", "rating": "s"})
    }

    #[tokio::test]
    async fn paginates_tag_search() {
        let first: Vec<_> = (1..=100).map(post).collect();
        let (root, server) = serve(vec![json(json!(first)), json(json!([post(101)]))]);
        let http = http("yande.re", root);
        let files = media(extract(
            "yandere",
            &http,
            Url::parse("https://yande.re/post?tags=a+b").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 101);
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "a b");
        assert_eq!(k["filename"], "yande.re 1 tag");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "yandere_1_md51.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("yandere/a b")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["a b","1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /post.json?tags=a+b&page=1&limit=100 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /post.json?tags=a+b&page=2&limit=100 "));
    }

    #[tokio::test]
    async fn popular_archive_uses_scale_initial() {
        let (root, _server) = serve(vec![json(json!([post(5)]))]);
        let http = http("yande.re", root);
        let files = media(extract(
            "yandere",
            &http,
            Url::parse("https://yande.re/post/popular_by_day?year=2020&month=1&day=2").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["scale"], "day");
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["P_{scale[0]}_{date}_{id}",["d","2017-07-14 02:40:00","5"]]"#
        );
    }
}
