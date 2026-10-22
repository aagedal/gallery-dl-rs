//! 3dbooru (behoimi.org), mirroring gallery_dl/extractor/3dbooru.py: the
//! Moebooru extractors of gallery_dl/extractor/moebooru.py with the
//! `/post/index.json` API and a fixed `Referer`.
use super::{booru_fields, date_value, item, leading_digits, parse_query, per_post};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "http://behoimi.org";
const REFERER: &str = "http://behoimi.org/post/show/";
/// `BooruExtractor.per_page`; Moebooru does not override it.
const PER_PAGE: usize = 100;
const FILENAME: &str = "{category}_{id}_{md5}.{extension}";
const HEADERS: &[(&str, &str)] = &[("Referer", REFERER), ("Accept-Encoding", "identity")];

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

pub static THREEDBOORU: Site = Site {
    category: "3dbooru",
    name: "3dbooru",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &POOL, &POST, &POPULAR],
    api_key: false,
};

#[derive(Debug, PartialEq)]
enum Target {
    Tag(String),
    Pool(String),
    Post(String),
    Popular { scale: String, query: String },
}

/// The upstream patterns, `(?:https?://)?(?:www\.)?behoimi\.org/...`,
/// matched against the URL's path, query and fragment.
fn target(url: &Url) -> Option<Target> {
    let rest = super::below(url, &["behoimi.org"]).or_else(|| super::below(url, &["www.behoimi.org"]))?;
    let rest = match url.fragment() {
        Some(fragment) => format!("{rest}#{fragment}"),
        None => rest,
    };
    // `/post(?:/(?:index)?)?\?tags=([^&#]+)`
    for prefix in ["/post?tags=", "/post/?tags=", "/post/index?tags="] {
        if let Some(tail) = rest.strip_prefix(prefix) {
            let tags = super::until(tail, &['&', '#']);
            return (!tags.is_empty()).then(|| Target::Tag(super::unquote_plus(tags)));
        }
    }
    if let Some(tail) = rest.strip_prefix("/pool/show/") {
        return leading_digits(tail).map(|id| Target::Pool(id.into()));
    }
    if let Some(tail) = rest.strip_prefix("/post/show/") {
        return leading_digits(tail).map(|id| Target::Post(id.into()));
    }
    // `/post/popular_(by_(?:day|week|month)|recent)(?:\?([^#]*))?`
    let tail = rest.strip_prefix("/post/popular_")?;
    let scale = ["by_day", "by_week", "by_month", "recent"]
        .into_iter()
        .find(|scale| tail.starts_with(scale))?;
    let query = tail[scale.len()..]
        .strip_prefix('?')
        .map(|q| super::until(q, &['#']).to_owned())
        .unwrap_or_default();
    Some(Target::Popular {
        scale: scale.into(),
        query,
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported 3dbooru URL"));
    };
    match target {
        Target::Tag(tags) => {
            let mut data = Map::new();
            data.insert("search_tags".into(), tags.clone().into());
            paginate(http, &TAG, tags, data)
        }
        Target::Pool(id) => crate::sites::once(async move {
            let mut data = Map::new();
            data.insert("pool".into(), pool_metadata(http, &id).await?);
            Ok(paginate(http, &POOL, format!("pool:{id}"), data))
        }),
        Target::Post(id) => paginate(http, &POST, format!("id:{id}"), Map::new()),
        Target::Popular { scale, query } => crate::sites::once(async move {
            let params = parse_query(&query, false);
            let data = popular_metadata(&scale, &params, today())?;
            let mut url = http.api(&format!("{ROOT}/post/popular_{scale}.json"))?;
            if !params.is_empty() {
                url.query_pairs_mut().extend_pairs(&params);
            }
            let posts = http.json_with(&url, HEADERS).await?;
            Ok(crate::sites::events(posts_events(http, &POPULAR, &posts, &data)))
        }),
    }
}

/// `MoebooruPoolExtractor.metadata`: the pool id, or the pool object with
/// the `metadata` option.
async fn pool_metadata(http: &Http, id: &str) -> Result<Value> {
    if !http.option_bool("metadata", false) {
        return Ok(super::int_value(id));
    }
    let url = http.api(&format!("{ROOT}/pool/show/{id}.json"))?;
    let mut pool = http.json_with(&url, HEADERS).await?;
    let object = pool
        .as_object_mut()
        .ok_or_else(|| error("3dbooru pool response is not an object"))?;
    if let Some(name) = object.get("name").and_then(Value::as_str) {
        let name = name.replace('_', " ");
        object.insert("name".into(), name.into());
    }
    object.remove("posts");
    Ok(pool)
}

fn today() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

/// `MoebooruPopularExtractor.metadata`; weekly views start on Monday.
fn popular_metadata(
    scale: &str,
    params: &[(String, String)],
    today: time::Date,
) -> Result<Map<String, Value>> {
    let get = |key| super::query_get(params, key);
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
            .map_err(|_| error("invalid 3dbooru popular date"))?;
        let monday = day - time::Duration::days(day.weekday().number_days_from_monday().into());
        date = format!(
            "{:04}-{:02}-{:02}",
            monday.year(),
            u8::from(monday.month()),
            monday.day()
        );
    } else if scale == "month" {
        let keep = date.chars().count().saturating_sub(3);
        date = date.chars().take(keep).collect();
    }
    let mut data = Map::new();
    data.insert("date".into(), date.into());
    data.insert("scale".into(), scale.into());
    Ok(data)
}

/// `MoebooruExtractor._pagination` over `/post/index.json`.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    tags: String,
    data: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(1u64), move |page| {
        let tags = tags.clone();
        let data = data.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/post/index.json"))?;
            url.query_pairs_mut()
                .append_pair("tags", &tags)
                .append_pair("page", &page.to_string())
                .append_pair("limit", &PER_PAGE.to_string());
            let posts = http.json_with(&url, HEADERS).await?;
            let count = posts.as_array().map_or(0, Vec::len);
            let events = posts_events(http, route, &posts, &data);
            let next = (count >= PER_PAGE && page < 10_000).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn posts_events(
    http: &Http,
    route: &'static Route,
    posts: &Value,
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let items = posts
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|post| parse_post(post, data))
        .collect();
    per_post(&THREEDBOORU, http, route, items)
}

/// Python `int(value)` for Unix timestamps given as numbers or strings.
fn python_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && f.abs() < 9.2e18)
                .map(|f| f.trunc() as i64)
        }),
        Value::String(s) => s.trim().replace('_', "").parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// One API post as `BooruExtractor.items` prepares it.
fn parse_post(post: &Value, data: &Map<String, Value>) -> Option<crate::sites::Item> {
    let mut fields = post.as_object()?.clone();
    let url = super::file_url(ROOT, fields.get("file_url")?.as_str()?)?;
    booru_fields(&url, &mut fields, data);
    let created = fields.get("created_at").and_then(python_int);
    fields.insert("date".into(), date_value(created));
    let mut item = item(url, fields, "author", "tags", created);
    for (name, value) in HEADERS {
        item = item.header(name, value);
    }
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("http://behoimi.org/post?tags=himekawa_azuru+dress"),
            Some(Target::Tag("himekawa_azuru dress".into()))
        );
        assert_eq!(
            t("https://www.behoimi.org/post/index?tags=a%20b&page=2"),
            Some(Target::Tag("a b".into()))
        );
        assert_eq!(
            t("http://behoimi.org/pool/show/27"),
            Some(Target::Pool("27".into()))
        );
        assert_eq!(
            t("http://behoimi.org/post/show/140852/tags"),
            Some(Target::Post("140852".into()))
        );
        assert_eq!(
            t("http://behoimi.org/post/popular_by_month?month=2&year=2013"),
            Some(Target::Popular {
                scale: "by_month".into(),
                query: "month=2&year=2013".into()
            })
        );
        assert_eq!(
            t("http://behoimi.org/post/popular_recent"),
            Some(Target::Popular {
                scale: "recent".into(),
                query: String::new()
            })
        );
        for raw in [
            "http://behoimi.org/post?page=2&tags=a",
            "http://behoimi.org/post?tags=",
            "http://behoimi.org/post/index.json?tags=a",
            "http://behoimi.org/post/show/x",
            "http://behoimi.org/post/popular_by_year",
            "http://behoimi.org.evil.test/post/show/1",
            "http://evilbehoimi.org/post/show/1",
            "http://behoimi.org:8080/post/show/1",
            "http://user@behoimi.org/post/show/1",
            "http://sub.behoimi.org/post/show/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert!(
            crate::sites::find(&Url::parse("http://behoimi.org/post/show/1").unwrap())
                .is_some_and(|site| site.category == "3dbooru")
        );
    }

    #[test]
    fn popular_dates_match_upstream() {
        let today = time::macros::date!(2026 - 10 - 22);
        let week = popular_metadata(
            "by_week",
            &parse_query("year=2019&month=8&day=22", false),
            today,
        )
        .unwrap();
        assert_eq!(week["date"], "2019-08-19");
        assert_eq!(week["scale"], "week");
        let month =
            popular_metadata("by_month", &parse_query("month=2&year=2013", false), today).unwrap();
        assert_eq!(month["date"], "2013-02");
        assert_eq!(month["scale"], "month");
        let recent = popular_metadata("recent", &[], today).unwrap();
        assert_eq!(recent["date"], "2026-10-22");
        assert_eq!(recent["scale"], "recent");
    }

    fn post(id: u64) -> Value {
        json!({"id": id, "md5": format!("{id:032x}"), "created_at": 1500000000,
            "file_url": format!("/data/ab/cd/{id:032x}.JPG"), "author": "up",
            "tags": "a b", "rating": "s"})
    }

    #[tokio::test]
    async fn paginates_tag_search_with_referer() {
        let first: Vec<_> = (1..=100).map(post).collect();
        let (root, server) = serve(vec![json(json!(first)), json(json!([post(101)]))]);
        let http = http("3dbooru", root);
        let files = media(extract(
            &http,
            Url::parse("http://behoimi.org/post?tags=a+b").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 101);
        let file = &files[0];
        assert_eq!(
            file.url.as_str(),
            "http://behoimi.org/data/ab/cd/00000000000000000000000000000001.JPG"
        );
        assert_eq!(file.headers["Referer"], REFERER);
        let k = keywords(file);
        assert_eq!(k["search_tags"], "a b");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["subcategory"], "tag");
        assert_eq!(file.metadata.as_ref().unwrap().timestamp, Some(1_500_000_000));
        let (name, dir) = crate::sites::default_templates(file).unwrap();
        assert_eq!(
            name.render(file).unwrap(),
            "3dbooru_1_00000000000000000000000000000001.jpg"
        );
        assert_eq!(
            dir.render(file).unwrap().unwrap(),
            std::path::PathBuf::from("3dbooru/a b")
        );
        let (namespace, identity) = crate::sites::archive_key(file).unwrap();
        assert_eq!(namespace, "3dbooru");
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["a b","1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /post/index.json?tags=a+b&page=1&limit=100 "),
            "{}",
            requests[0]
        );
        assert!(requests[0].to_ascii_lowercase().contains("referer: http://behoimi.org/post/show/"));
        assert!(requests[1].starts_with("GET /post/index.json?tags=a+b&page=2&limit=100 "));
    }

    #[tokio::test]
    async fn pool_post_and_popular_requests() {
        let mut odd = post(7);
        odd["created_at"] = json!({"json_class": "Time", "s": 1});
        let (root, server) = serve(vec![
            json(json!([post(5)])),
            json(json!([odd])),
            json(json!([post(9)])),
        ]);
        let http = http("3dbooru", root);
        let pool = media(extract(
            &http,
            Url::parse("http://behoimi.org/pool/show/27").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&pool[0])["pool"], 27);
        let (_, identity) = crate::sites::archive_key(&pool[0]).unwrap();
        assert_eq!(identity, r#"["p_{pool}_{id}",["27","5"]]"#);
        let (_, dir) = crate::sites::default_templates(&pool[0]).unwrap();
        assert_eq!(
            dir.render(&pool[0]).unwrap().unwrap(),
            std::path::PathBuf::from("3dbooru/pool/27")
        );
        let single = media(extract(
            &http,
            Url::parse("http://behoimi.org/post/show/7").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&single[0])["date"], "[Invalid DateTime]");
        assert_eq!(crate::sites::archive_key(&single[0]).unwrap().1, r#"["{id}",["7"]]"#);
        let popular = media(extract(
            &http,
            Url::parse("http://behoimi.org/post/popular_by_day?year=2013&month=2&day=3").unwrap(),
        ))
        .await;
        let k = keywords(&popular[0]);
        assert_eq!(k["scale"], "day");
        // `_prepare` replaces the view date with the post date, as upstream.
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(
            crate::sites::archive_key(&popular[0]).unwrap().1,
            r#"["P_{scale[0]}_{date}_{id}",["d","2017-07-14 02:40:00","9"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/index.json?tags=pool%3A27&page=1&limit=100 "));
        assert!(requests[1].starts_with("GET /post/index.json?tags=id%3A7&page=1&limit=100 "));
        assert!(
            requests[2].starts_with("GET /post/popular_by_day.json?year=2013&month=2&day=3 "),
            "{}",
            requests[2]
        );
    }
}
