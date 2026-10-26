//! WEBTOON episodes, comics and artists, mirroring
//! gallery_dl/extractor/webtoons.py.
//!
//! Not ported: the `thumbnails`, `banners` and `bgm` options (extra assets;
//! background music needs ytdl/HLS handoff) and the age-gate redirect check.
use super::{Gallery, keywords, set, text};
use crate::sites::media::{parse_query, query_get};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use text::parse_int;
use url::Url;

pub static SITE: Site = Site {
    category: "webtoons",
    name: "WEBTOON",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&EPISODE, &COMIC, &ARTIST],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://www.webtoons.com";
const DIRECTORY: &[&str] = &["{category}", "{comic}"];
const FILENAME: &str = "{episode_no}-{num:>02}{type:?-//}.{extension}";
const ARCHIVE: &str = "{title_no}_{episode_no}_{num}";
/// `WebtoonsBase.setup_agegate_cookies`.
const COOKIES: &str = "atGDPR=AD_CONSENT; needCCPA=false; needCOPPA=false; \
                       needGDPR=false; pagGDPR=true; ageGatePass=true";

static EPISODE: Route = Route::new("episode", FILENAME, DIRECTORY, ARCHIVE);
static COMIC: Route = Route::new(
    "comic",
    "{type}.{extension}",
    DIRECTORY,
    "{title_no}_{type}",
);
static ARTIST: Route = Route::new("artist", FILENAME, DIRECTORY, ARCHIVE);

/// `util.CODES`, for `code_to_language`.
const LANGUAGES: &[(&str, &str)] = &[
    ("ar", "Arabic"),
    ("bg", "Bulgarian"),
    ("bn", "Bengali"),
    ("ca", "Catalan"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("de", "German"),
    ("el", "Greek"),
    ("en", "English"),
    ("es", "Spanish"),
    ("fa", "Persian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hu", "Hungarian"),
    ("id", "Indonesian"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("ms", "Malay"),
    ("nl", "Dutch"),
    ("no", "Norwegian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("sr", "Serbian"),
    ("sv", "Swedish"),
    ("th", "Thai"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("vi", "Vietnamese"),
    ("zh", "Chinese"),
];

pub(crate) fn code_to_language(code: &str) -> Option<&'static str> {
    let code = code.to_lowercase();
    LANGUAGES.iter().find(|(c, _)| *c == code).map(|(_, l)| *l)
}

/// `text.extract_from`: successive extractions from one position; a miss
/// returns `""` and leaves the position alone.
struct Extr<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Extr<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn find(&mut self, begin: &str, end: &str) -> Option<&'a str> {
        let first = self.text[self.pos..].find(begin)? + self.pos + begin.len();
        let last = self.text[first..].find(end)? + first;
        self.pos = last + end.len();
        Some(&self.text[first..last])
    }

    fn get(&mut self, begin: &str, end: &str) -> &'a str {
        self.find(begin, end).unwrap_or_default()
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    /// `base` is `LANG/GENRE/COMIC/EPISODE`.
    Episode {
        base: String,
        lang: String,
        genre: String,
        comic: String,
        query: String,
    },
    /// `base` is `LANG/GENRE/COMIC`.
    Comic {
        base: String,
        lang: String,
        genre: String,
        comic: String,
        query: String,
    },
    Artist {
        lang: String,
        artist: String,
    },
}
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !crate::sites::host_is(url, &["webtoons.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let query = url.query().unwrap_or_default();
    match raw.as_slice() {
        [lang, genre, comic, episode, "viewer"]
            if [lang, genre, comic, episode].iter().all(|s| !s.is_empty())
                && !query.is_empty()
                && !query.contains(['\'', '"']) =>
        {
            Some(Target::Episode {
                base: format!("{lang}/{genre}/{comic}/{episode}"),
                lang: (*lang).into(),
                genre: (*genre).into(),
                comic: (*comic).into(),
                query: query.into(),
            })
        }
        [lang, genre, comic, "list"]
            if [lang, genre, comic].iter().all(|s| !s.is_empty()) && !query.is_empty() =>
        {
            Some(Target::Comic {
                base: format!("{lang}/{genre}/{comic}"),
                lang: (*lang).into(),
                genre: (*genre).into(),
                comic: (*comic).into(),
                query: query.into(),
            })
        }
        ["p", "community", lang, "u", artist, ..] if !lang.is_empty() && !artist.is_empty() => {
            Some(Target::Artist {
                lang: (*lang).into(),
                artist: (*artist).into(),
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Episode {
            base,
            lang,
            genre,
            comic,
            query,
        }) => crate::sites::once(async move {
            let page = fetch(http, &format!("{ROOT}/{base}/viewer?{query}")).await?;
            Ok(crate::sites::events(episode_events(
                http, &page, &lang, &genre, &comic, &query,
            )))
        }),
        Some(Target::Comic {
            base,
            lang,
            genre,
            comic,
            query,
        }) => comic_events(http, base, lang, genre, comic, query),
        Some(Target::Artist { lang, artist }) => {
            crate::sites::once(async move { artist_events(http, &lang, &artist).await })
        }
        None => crate::sites::fail(error("unsupported WEBTOON URL")),
    }
}

/// A page with the age-gate cookies and Referer upstream sends.
async fn fetch(http: &Http, url: &str) -> Result<String> {
    http.text_with(
        &http.api(url)?,
        &[
            ("Cookie", COOKIES),
            ("Referer", "https://www.webtoons.com/"),
        ],
    )
    .await
}

/// `WebtoonsEpisodeExtractor.metadata`.
fn metadata(page: &str, lang: &str, genre: &str, comic: &str, query: &str) -> Map<String, Value> {
    let params = parse_query(Some(query));
    let param = |name| query_get(&params, name).map_or(Value::Null, Value::from);
    let mut e = Extr::new(page);
    let title = e.get(r#"<meta property="og:title" content=""#, "\"");
    let descr = e.get(r#"<meta property="og:description" content=""#, "\"");
    let (comic_name, episode_name) = if e.get(r#"<div class="subj_info""#, "\n").is_empty() {
        ("", "")
    } else {
        (
            e.get(">", "<"),
            e.get(r#"<h1 class="subj_episode" title=""#, "\""),
        )
    };
    let episode = if e
        .get(r#"<span class="tx _btnOpenEpisodeLis"#, "\"")
        .is_empty()
    {
        ""
    } else {
        e.get(">#", "<")
    };
    let (username, author_name) = if e.get(r#"<span class="author""#, "\n").is_empty() {
        ("", "")
    } else {
        (e.get("/u/", "\""), e.get("<span>", "</span>"))
    };
    let unescape = |s: &str| Value::from(crate::sites::unescape(s));
    keywords([
        ("genre", genre.into()),
        ("comic", comic.into()),
        ("title_no", param("title_no")),
        ("episode_no", param("episode_no")),
        ("title", unescape(title)),
        ("episode", episode.into()),
        ("comic_name", unescape(comic_name)),
        ("episode_name", unescape(episode_name)),
        ("username", username.into()),
        ("author_name", unescape(author_name)),
        ("description", unescape(descr)),
        ("lang", lang.into()),
        (
            "language",
            code_to_language(lang).map_or(Value::Null, Value::from),
        ),
    ])
}

/// `_url`: the `swebtoon-phinf` image host serves without hotlink checks.
fn image_host(url: &str) -> String {
    url.replace("://webtoon-phinf.", "://swebtoon-phinf.")
}

/// Python truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The `quality` option as per-extension `type` parameters, where `false`
/// drops the query to request originals. `None` leaves URLs unchanged.
fn quality(http: &Http) -> Option<Map<String, Value>> {
    let original = || {
        Some(keywords([
            ("jpg", false.into()),
            ("jpeg", false.into()),
            ("webp", false.into()),
        ]))
    };
    let both = |value: Value| Some(keywords([("jpg", value.clone()), ("jpeg", value)]));
    match http.option("quality") {
        None | Some(Value::Null) => original(),
        Some(Value::String(s)) if s == "original" => original(),
        Some(value) if !truthy(value) => None,
        Some(Value::String(s)) => both(s.clone().into()),
        Some(Value::Bool(b)) => both(format!("q{}", crate::sites::scalar(&Value::Bool(*b))).into()),
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => both(format!("q{n}").into()),
        Some(Value::Object(map)) => Some(map.clone()),
        Some(_) => None,
    }
}

/// `WebtoonsEpisodeExtractor.images`.
fn images(page: &str, quality: Option<&Map<String, Value>>) -> Vec<String> {
    let mut urls = Vec::new();
    let mut extr = Extr::new(page);
    while let Some(url) = extr.find(r#"class="_images" data-url=""#, "\"") {
        let path = url.rsplit_once('?').map_or(url, |(path, _)| path);
        let ext = path
            .rsplit_once('.')
            .map_or(path, |(_, e)| e)
            .to_lowercase();
        let kind = quality.and_then(|map| map.get(&ext));
        let url = match kind {
            Some(Value::Bool(false)) => path.to_owned(),
            Some(kind) if truthy(kind) => format!("{path}?type={}", crate::sites::scalar(kind)),
            _ => url.to_owned(),
        };
        urls.push(image_host(&url));
    }
    urls
}

fn episode_events(
    http: &Http,
    page: &str,
    lang: &str,
    genre: &str,
    comic: &str,
    query: &str,
) -> Vec<crate::event::ExtractionEvent> {
    let data = metadata(page, lang, genre, comic, query);
    let id = format!(
        "{}_{}",
        data.get("title_no")
            .map(crate::sites::scalar)
            .unwrap_or_default(),
        data.get("episode_no")
            .map(crate::sites::scalar)
            .unwrap_or_default()
    );
    let pages = images(page, quality(http).as_ref())
        .into_iter()
        .map(|url| (url, None))
        .collect();
    let mut gallery = Gallery::chapter(&EPISODE, data, pages)
        .id(id)
        .header("Referer", "https://www.webtoons.com/");
    gallery.enum_key = "num";
    gallery.events(&SITE, http)
}

/// `WebtoonsComicExtractor.get_episode_urls`: every episode URL in the
/// episode list, as `EPISODE.pattern.finditer` finds them.
fn episode_urls(page: &str) -> Vec<Url> {
    let mut extr = Extr::new(page);
    let list = extr.get(r#"id="_listUl""#, "</ul>");
    let mut urls = Vec::new();
    let mut search = 0;
    while let Some(found) = list[search..].find("webtoons.com/").map(|i| search + i) {
        search = found + 1;
        let mut start = found;
        start -= if list[..start].ends_with("www.") {
            4
        } else {
            0
        };
        for scheme in ["https://", "http://"] {
            if list[..start].ends_with(scheme) {
                start -= scheme.len();
                break;
            }
        }
        let end = list[found..]
            .find(['#', '\'', '"'])
            .map_or(list.len(), |i| found + i);
        let raw = text::ensure_http_scheme(&list[start..end]);
        if let Ok(url) = Url::parse(&raw)
            && matches!(target(&url), Some(Target::Episode { .. }))
        {
            urls.push(url);
        }
    }
    urls
}

/// `WebtoonsComicExtractor.items`: follow list pages while the next page
/// is linked.
fn comic_events(
    http: &Http,
    base: String,
    lang: String,
    genre: String,
    comic: String,
    query: String,
) -> Events<'_> {
    let params = parse_query(Some(&query));
    let title_no = parse_int(query_get(&params, "title_no").unwrap_or_default());
    let first = match query_get(&params, "page") {
        Some(page) if !page.is_empty() => parse_int(page),
        _ => 1,
    };
    let path = move |page_no: i64| format!("/{base}/list?title_no={title_no}&page={page_no}");
    crate::sites::paginate(Some(first), move |state| {
        let (path, lang, genre, comic) = (path.clone(), lang.clone(), genre.clone(), comic.clone());
        async move {
            let Some(page_no) = state else {
                return Ok(None);
            };
            let page = fetch(http, &format!("{ROOT}{}", path(page_no))).await?;
            let mut kw = keywords([
                ("lang", lang.into()),
                ("genre", genre.into()),
                ("comic", comic.into()),
                ("title_no", title_no.into()),
                ("page", page_no.into()),
            ]);
            let children = episode_urls(&page)
                .into_iter()
                .map(|url| {
                    let params = parse_query(url.query());
                    let episode_no =
                        parse_int(query_get(&params, "episode_no").unwrap_or_default());
                    set(&mut kw, "episode_no", episode_no);
                    (url, kw.clone())
                })
                .collect();
            let events = super::queue_urls(http, &SITE, &COMIC, children);
            let next = page_no + 1;
            let more = page.contains(&path(next)) && next < 10_000;
            Ok(Some((events, more.then_some(next))))
        }
    })
}

/// `WebtoonsArtistExtractor`: the creator's titles from the community API.
async fn artist_events<'a>(http: &'a Http, lang: &str, artist: &str) -> Result<Events<'a>> {
    let language = code_to_language(lang)
        .map(str::to_uppercase)
        .ok_or_else(|| error("unsupported WEBTOON community language"))?;
    let page = fetch(http, &format!("{ROOT}/p/community/{lang}/u/{artist}")).await?;
    let creator = Extr::new(&page).get(r#"\"creatorId\":\""#, "\\");
    if creator.is_empty()
        || !creator
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(error("WEBTOON creator ID not found"));
    }
    let mut url = http.api(&format!(
        "{ROOT}/p/community/api/v1/creator/{creator}/titles"
    ))?;
    url.query_pairs_mut()
        .append_pair("language", &language)
        .append_pair("nextSize", "50");
    let data = http
        .json_with(
            &url,
            &[
                ("Cookie", COOKIES),
                ("Referer", "https://www.webtoons.com/"),
                ("language", &language),
            ],
        )
        .await?;
    let children = data
        .pointer("/result/titles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|comic| {
            let path = comic.pointer("/extra/episodeListPath")?.as_str()?;
            let url = Url::parse(&format!("{ROOT}{path}")).ok()?;
            Some((url, comic.as_object()?.clone()))
        })
        .collect();
    Ok(super::queue_urls(http, &SITE, &ARTIST, children))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, json, keywords as kw, media, response, serve};
    use futures_util::StreamExt;
    use serde_json::json;

    const EPISODE_URL: &str = "https://www.webtoons.com/en/fantasy/tower-of-god/season-3-ep-1/viewer?title_no=95&episode_no=1";

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert!(matches!(
            t(EPISODE_URL),
            Some(Target::Episode { ref base, ref lang, .. })
                if base == "en/fantasy/tower-of-god/season-3-ep-1" && lang == "en"
        ));
        assert!(matches!(
            t("https://webtoons.com/en/fantasy/tower-of-god/list?title_no=95&page=2"),
            Some(Target::Comic { ref comic, .. }) if comic == "tower-of-god"
        ));
        assert_eq!(
            t("https://www.webtoons.com/p/community/en/u/abc123"),
            Some(Target::Artist {
                lang: "en".into(),
                artist: "abc123".into()
            })
        );
        for raw in [
            "https://www.webtoons.com/en/fantasy/tower-of-god/list",
            "https://www.webtoons.com/en/fantasy/tower-of-god/ep/viewer",
            "https://www.webtoons.com/en/fantasy/viewer?title_no=1",
            "https://www.webtoons.com.evil.test/en/a/b/list?title_no=1",
            "https://m.webtoons.com/en/a/b/list?title_no=1",
            "https://www.webtoons.com:444/en/a/b/list?title_no=1",
            "https://u:p@www.webtoons.com/en/a/b/list?title_no=1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = r#"<meta property="og:title" content="Season 3 Ep. 1"/>
<meta property="og:description" content="Tower &amp; God"/>
<div class="subj_info">
<a href="/x" class="subj">Tower of God</a>
<h1 class="subj_episode" title="[Season 3] Ep. 1">x</h1>
<span class="tx _btnOpenEpisodeList">#418</span>
<span class="author">
<a href="https://www.webtoons.com/p/community/en/u/slv">x</a><span>SIU</span>
<img class="_images" data-url="https://webtoon-phinf.pstatic.net/a/001.jpg?type=q90" />
<img class="_images" data-url="https://webtoon-phinf.pstatic.net/a/002.gif?type=q90" />
"#;

    #[tokio::test]
    async fn extracts_episode() {
        let (root, requests) = serve(vec![html(PAGE)]);
        let http = http("WEBTOON", root);
        let files = media(extract(&http, Url::parse(EPISODE_URL).unwrap())).await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://swebtoon-phinf.pstatic.net/a/001.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://swebtoon-phinf.pstatic.net/a/002.gif?type=q90"
        );
        assert_eq!(files[0].headers["Referer"], "https://www.webtoons.com/");
        let k = kw(&files[0]);
        assert_eq!(k["title"], "Season 3 Ep. 1");
        assert_eq!(k["description"], "Tower & God");
        assert_eq!(k["comic_name"], "Tower of God");
        assert_eq!(k["episode_name"], "[Season 3] Ep. 1");
        assert_eq!(k["episode"], "418");
        assert_eq!(k["username"], "slv");
        assert_eq!(k["author_name"], "SIU");
        assert_eq!(k["title_no"], "95");
        assert_eq!(k["episode_no"], "1");
        assert_eq!(k["language"], "English");
        assert_eq!(k["num"], 1);
        assert_eq!(k["count"], 2);
        assert_eq!(path(&files[1]), "webtoons/tower-of-god/1-02.gif");
        assert_eq!(
            archive(&files[1]),
            r#"["{title_no}_{episode_no}_{num}",["95","1","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /en/fantasy/tower-of-god/season-3-ep-1/viewer?title_no=95&episode_no=1 "
        ));
        assert!(requests[0].contains("ageGatePass=true"));
    }

    #[test]
    fn quality_option_selects_image_types() {
        let page = r#"class="_images" data-url="https://webtoon-phinf.x/1.JPG?type=q90""#;
        let q = |value: Value| {
            let http =
                http("WEBTOON", Url::parse("http://127.0.0.1:1/").unwrap()).with_options(Some(
                    std::sync::Arc::new(json!({"quality": value}).as_object().unwrap().clone()),
                ));
            images(page, quality(&http).as_ref()).remove(0)
        };
        assert_eq!(q(json!(50)), "https://swebtoon-phinf.x/1.JPG?type=q50");
        assert_eq!(q(json!("x")), "https://swebtoon-phinf.x/1.JPG?type=x");
        assert_eq!(q(json!(false)), "https://swebtoon-phinf.x/1.JPG?type=q90");
        assert_eq!(q(json!({"jpg": false})), "https://swebtoon-phinf.x/1.JPG");
    }

    fn list(page: u32, next: bool) -> String {
        let more = if next {
            format!(
                r#"<a href="/en/fantasy/tower-of-god/list?title_no=95&page={}">"#,
                page + 1
            )
        } else {
            String::new()
        };
        format!(
            r#"<ul id="_listUl"><li><a href="https://www.webtoons.com/en/fantasy/tower-of-god/ep-{page}/viewer?title_no=95&episode_no={page}" class="x">
<a href="https://evil.test/en/a/b/c/viewer?title_no=1"></ul>{more}"#
        )
    }

    #[tokio::test]
    async fn comic_follows_list_pages() {
        let (root, requests) = serve(vec![
            html(&list(1, true)),
            html(PAGE),
            html(&list(2, false)),
            html(PAGE),
        ]);
        let http = http("WEBTOON", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.webtoons.com/en/fantasy/tower-of-god/list?title_no=95")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /en/fantasy/tower-of-god/list?title_no=95&page=1 "));
        assert!(
            requests[1]
                .starts_with("GET /en/fantasy/tower-of-god/ep-1/viewer?title_no=95&episode_no=1 ")
        );
        assert!(requests[2].starts_with("GET /en/fantasy/tower-of-god/list?title_no=95&page=2 "));
        assert!(requests[3].starts_with("GET /en/fantasy/tower-of-god/ep-2/viewer?"));
    }

    #[tokio::test]
    async fn artist_queues_comics() {
        let (root, requests) = serve(vec![
            html(r#"x \"creatorId\":\"c-1\"} y"#),
            json(json!({"result": {"titles": [
                {"title": "T", "extra": {"episodeListPath": "/en/fantasy/t/list?title_no=7"}}]}})),
            html(&list(1, false)),
            html(PAGE),
        ]);
        let http = http("WEBTOON", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.webtoons.com/p/community/en/u/abc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /p/community/en/u/abc "));
        assert!(requests[1].starts_with(
            "GET /p/community/api/v1/creator/c-1/titles?language=ENGLISH&nextSize=50 "
        ));
        assert!(requests[1].contains("language: ENGLISH"));
        assert!(requests[2].starts_with("GET /en/fantasy/t/list?title_no=7&page=1 "));
    }

    #[tokio::test]
    async fn artist_without_creator_id_fails() {
        let (root, requests) = serve(vec![html("<html>no creator</html>")]);
        let http = http("WEBTOON", root);
        let events = extract(
            &http,
            Url::parse("https://www.webtoons.com/p/community/en/u/abc").unwrap(),
        );
        let results = events.collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("creator ID")
        );
        requests.join().unwrap();
    }

    #[tokio::test]
    async fn missing_episode_is_an_error() {
        let (root, requests) = serve(vec![response("404 Not Found", "text/html", b"")]);
        let http = http("WEBTOON", root);
        let events = extract(&http, Url::parse(EPISODE_URL).unwrap());
        let results = events.collect::<Vec<_>>().await;
        assert!(results.iter().any(Result::is_err));
        requests.join().unwrap();
    }
}
