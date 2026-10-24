//! MangaPark chapters and manga, mirroring gallery_dl/extractor/mangapark.py
//! through the site's GraphQL API on whichever mirror the URL names.
use super::{Gallery, Memo, get, keywords, text};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "mangapark",
    name: "MangaPark",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &[
        "{category}",
        "{manga}",
        "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
    ],
    "{manga}_{chapter}{chapter_minor}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

/// `BASE_PATTERN` hosts, each also with `www.`.
const HOSTS: &[&str] = &[
    "mangapark.com",
    "mangapark.net",
    "mangapark.org",
    "mangapark.me",
    "mangapark.io",
    "mangapark.to",
    "comicpark.com",
    "comicpark.net",
    "comicpark.org",
    "comicpark.me",
    "comicpark.io",
    "comicpark.to",
    "readpark.com",
    "readpark.net",
    "readpark.org",
    "readpark.me",
    "readpark.io",
    "readpark.to",
    "parkmanga.com",
    "parkmanga.net",
    "parkmanga.org",
    "mpark.to",
];

/// `utils/mangapark_graphql.py`.
const GET_COMIC_CHAPTER_LIST: &str = "
query Get_comicChapterList($comicId: ID!) {
    get_comicChapterList(comicId: $comicId) {
        data {
            id
            dname
            title
            lang
            urlPath
            srcTitle
            sourceId
            dateCreate
        }
    }
}
";
const GET_CHAPTER_NODE: &str = "
query Get_chapterNode($getChapterNodeId: ID!) {
    get_chapterNode(id: $getChapterNodeId) {
        data {
            id
            dname
            lang
            sourceId
            srcTitle
            dateCreate
            comicNode{
                id
            }
            imageFile {
                urlList
            }
        }
    }
}
";
const GET_COMIC_NODE: &str = "
query Get_comicNode($getComicNodeId: ID!) {
    get_comicNode(id: $getComicNodeId) {
        data {
            id
            name
            artists
            authors
            genres
        }
    }
}
";
const GET_CONTENT_SOURCE_CHAPTER_LIST: &str = "
  query get_content_source_chapterList($sourceId: Int!) {
    get_content_source_chapterList(
      sourceId: $sourceId
    ) {

  id
  data {


  id
  sourceId

  dbStatus
  isNormal
  isHidden
  isDeleted
  isFinal

  dateCreate
  datePublic
  dateModify
  lang
  volume
  serial
  dname
  title
  urlPath

  srcTitle srcColor

  count_images

  stat_count_post_child
  stat_count_post_reply
  stat_count_views_login
  stat_count_views_guest

  userId
  userNode {

  id
  data {

id
name
uniq
avatarUrl
urlPath

verified
deleted
banned

dateCreate
dateOnline

stat_count_chapters_normal
stat_count_chapters_others

is_adm is_mod is_vip is_upr

  }

  }

  disqusId


  }

    }
  }
";
const GET_CONTENT_COMIC_SOURCES: &str = "
  query get_content_comic_sources($comicId: Int!, $dbStatuss: [String] = [], $userId: Int, $haveChapter: Boolean, $sortFor: String) {
    get_content_comic_sources(
      comicId: $comicId
      dbStatuss: $dbStatuss
      userId: $userId
      haveChapter: $haveChapter
      sortFor: $sortFor
    ) {

id
data{

  id

  dbStatus
  isNormal
  isHidden
  isDeleted

  lang name altNames authors artists

  release
  genres summary{code} extraInfo{code}

  urlCover600
  urlCover300
  urlCoverOri

  srcTitle srcColor

  chapterCount
  chapterNode_last {
    id
    data {
      dateCreate datePublic dateModify
      volume serial
      dname title
      urlPath
      userNode {
        id data {uniq name}
      }
    }
  }
}

    }
  }
";

#[derive(Debug, PartialEq)]
enum Target {
    Chapter { root: String, id: String },
    Manga { root: String, id: String },
}

fn digits(text: &str) -> Option<&str> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (1..=20).contains(&end).then(|| &text[..end])
}

/// `/(?:title/[^/?#]+/|comic/\d+/[^/?#]+/[^/?#]+-i)(\d+)` for chapters,
/// `/(?:title|comic)/(\d+)(?:[/-][^/?#]*)?/?$` for manga. `root` is the
/// URL's own origin (`text.root_from_url`).
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !crate::sites::host_is(url, HOSTS) {
        return None;
    }
    let root = format!("{}://{}", url.scheme(), url.host_str()?);
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let chapter = match raw.as_slice() {
        ["title", manga, chapter, ..] if !manga.is_empty() => digits(chapter),
        // `[^/?#]+-i(\d+)` backtracks to the last `-i` followed by digits.
        ["comic", number, manga, chapter, ..]
            if digits(number) == Some(*number) && !manga.is_empty() =>
        {
            chapter
                .rmatch_indices("-i")
                .filter(|(i, _)| *i > 0)
                .find_map(|(i, _)| digits(&chapter[i + 2..]))
        }
        _ => None,
    };
    if let Some(id) = chapter {
        return Some(Target::Chapter {
            root,
            id: id.to_owned(),
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let (kind, rest) = match raw.as_slice() {
        [kind, rest] | [kind, rest, ""] => (*kind, (*rest).to_owned()),
        [kind, number, name] | [kind, number, name, ""] if digits(number) == Some(*number) => {
            (*kind, format!("{number}/{name}"))
        }
        _ => return None,
    };
    let id = digits(&rest)?;
    let tail = &rest[id.len()..];
    (matches!(kind, "title" | "comic") && (tail.is_empty() || tail.starts_with(['-', '/']))).then(
        || Target::Manga {
            root,
            id: id.to_owned(),
        },
    )
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { root, id }) => chapter_events(http, root, id, Memo::default()),
        Some(Target::Manga { root, id }) => {
            crate::sites::once(async move { manga_events(http, root, id).await })
        }
        None => crate::sites::fail(error("unsupported MangaPark URL")),
    }
}

/// `_request_graphql`: the value of the response's only `data` field.
async fn graphql(
    http: &Http,
    root: &str,
    operation: &str,
    query: &str,
    variables: Value,
) -> Result<Value> {
    let body = json!({
        "query": query,
        "variables": variables,
        "operationName": operation,
    });
    let referer = format!("{root}/");
    let response = http
        .post_json(
            &http.api(&format!("{root}/apo/"))?,
            &body,
            &http.browser_headers(&[("Referer", &referer)]),
        )
        .await?;
    response
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.values().next_back())
        .cloned()
        .ok_or_else(|| error("MangaPark returned no data"))
}

/// `_parse_chapter_title`: `(?i)(?:vol(?:\.|ume)?\s*(\d+)\s*)?`
/// `ch(?:\.|apter)?\s*(\d+)([^\s:]*)(?:\s*:\s*(.*))?` at the start, or
/// `(0, 0, "", "")`.
fn parse_title(title: &str) -> (i64, i64, String, String) {
    fn word<'t>(s: &'t str, word: &str) -> Option<&'t str> {
        let head = s.get(..word.len())?;
        head.eq_ignore_ascii_case(word).then(|| &s[word.len()..])
    }
    fn number(s: &str) -> Option<(&str, &str)> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        (end > 0).then(|| s.split_at(end))
    }
    /// `PREFIX(?:\.|SUFFIX)?\s*(\d+)`.
    fn numbered<'t>(s: &'t str, prefix: &str, suffix: &str) -> Option<(&'t str, &'t str)> {
        let s = word(s, prefix)?;
        [word(s, "."), word(s, suffix), Some(s)]
            .into_iter()
            .flatten()
            .find_map(|s| number(s.trim_start()))
    }
    let (volume, rest) = match numbered(title, "vol", "ume") {
        Some((volume, rest)) => (volume, rest.trim_start()),
        None => ("", title),
    };
    let Some((chapter, rest)) = numbered(rest, "ch", "apter") else {
        return (0, 0, String::new(), String::new());
    };
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ':')
        .unwrap_or(rest.len());
    let (minor, rest) = rest.split_at(end);
    let name = rest
        .trim_start()
        .strip_prefix(':')
        .map(|name| name.trim_start().split('\n').next().unwrap_or_default())
        .unwrap_or_default();
    (
        text::parse_int(volume),
        text::parse_int(chapter),
        minor.to_owned(),
        name.to_owned(),
    )
}

/// The keywords both extractors take from a chapter node.
fn chapter_data(chapter: &Value) -> (Map<String, Value>, Option<i64>) {
    let (volume, number, minor, title) = parse_title(super::str_of(chapter, "dname"));
    let lang = match chapter.get("lang") {
        Some(Value::String(lang)) if !lang.is_empty() => lang.clone(),
        _ => "en".to_owned(),
    };
    let created = chapter
        .get("dateCreate")
        .and_then(Value::as_f64)
        .map(|ms| (ms / 1000.0).floor() as i64);
    let data = keywords([
        ("volume", volume.into()),
        ("chapter", number.into()),
        ("chapter_minor", minor.into()),
        ("title", title.into()),
        (
            "language",
            text::code_to_language(&lang).map_or(Value::Null, Value::from),
        ),
        ("lang", lang.into()),
        ("source", get(chapter, "srcTitle")),
        ("source_id", get(chapter, "sourceId")),
        ("date", text::date(created)),
    ]);
    (data, created)
}

/// `MangaparkChapterExtractor`: chapter node, its (memoized) comic node,
/// then the image list.
fn chapter_events(http: &Http, root: String, id: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let node = graphql(
            http,
            &root,
            "Get_chapterNode",
            GET_CHAPTER_NODE,
            json!({"getChapterNodeId": id}),
        )
        .await?;
        let chapter = node.get("data").unwrap_or(&Value::Null);
        let manga_id = chapter
            .pointer("/comicNode/id")
            .map(crate::sites::scalar)
            .ok_or_else(|| error("MangaPark chapter has no comic"))?;
        let manga = match memo.get(&manga_id) {
            Some(manga) => manga,
            None => {
                let node = graphql(
                    http,
                    &root,
                    "Get_comicNode",
                    GET_COMIC_NODE,
                    json!({"getComicNodeId": manga_id}),
                )
                .await?;
                let manga = node.get("data").cloned().unwrap_or(Value::Null);
                memo.insert(&manga_id, manga.clone());
                manga
            }
        };
        let (fields, created) = chapter_data(chapter);
        let mut data = keywords([
            ("manga", get(&manga, "name")),
            ("manga_id", text::parse_int_value(manga.get("id")).into()),
            ("artist", get(&manga, "artists")),
            ("author", get(&manga, "authors")),
            ("genre", get(&manga, "genres")),
        ]);
        data.extend(fields);
        data.insert(
            "chapter_id".into(),
            text::parse_int_value(chapter.get("id")).into(),
        );
        let pages = chapter
            .pointer("/imageFile/urlList")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|url| (url.to_owned(), None))
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages)
                .id(id)
                .timestamp(created)
                .header("Referer", format!("{root}/"))
                .events(&SITE, http),
        ))
    })
}

/// `MangaparkMangaExtractor.items`: every chapter, or those of the
/// configured `source`, queued with the chapter list's keywords.
async fn manga_events<'a>(http: &'a Http, root: String, id: String) -> Result<Events<'a>> {
    let manga_id = text::parse_int(&id);
    let mut chapters = match http.option("source") {
        Some(source) if crate::sites::art::truthy(Some(source)) => {
            let source_id = select_source(http, &root, manga_id, source).await?;
            graphql(
                http,
                &root,
                "get_content_source_chapterList",
                GET_CONTENT_SOURCE_CHAPTER_LIST,
                json!({"sourceId": source_id}),
            )
            .await?
        }
        _ => {
            graphql(
                http,
                &root,
                "Get_comicChapterList",
                GET_COMIC_CHAPTER_LIST,
                json!({"comicId": id}),
            )
            .await?
        }
    };
    let chapters = chapters
        .as_array_mut()
        .map(std::mem::take)
        .unwrap_or_default();
    let chapters: Vec<Value> = if http.option_bool("chapter-reverse", false) {
        chapters.into_iter().rev().collect()
    } else {
        chapters
    };
    let memo = Memo::default();
    let mut streams: Vec<Events<'a>> = Vec::new();
    for chapter in chapters {
        let chapter = chapter.get("data").unwrap_or(&Value::Null);
        let Ok(url) = Url::parse(&format!("{root}{}", super::str_of(chapter, "urlPath"))) else {
            continue;
        };
        let Some(Target::Chapter { id, .. }) = target(&url) else {
            continue;
        };
        let (fields, _) = chapter_data(chapter);
        let mut data = keywords([("manga_id", manga_id.into())]);
        data.extend(fields);
        data.insert("chapter_id".into(), get(chapter, "id"));
        let title = match chapter.get("title") {
            Some(title) if crate::sites::art::truthy(Some(title)) => title.clone(),
            _ => data.get("title").cloned().unwrap_or_default(),
        };
        data.insert("title".into(), title);
        let (root, memo) = (root.clone(), memo.clone());
        streams.push(super::queue_with(&SITE, &MANGA, &data, move || {
            chapter_events(http, root, id, memo)
        }));
    }
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

/// `_select_source`: an integer source ID, or the first source matching
/// `GROUP[:LANG]`.
async fn select_source(http: &Http, root: &str, manga_id: i64, source: &Value) -> Result<Value> {
    if source.is_i64() || source.is_u64() {
        return Ok(source.clone());
    }
    let source = crate::sites::scalar(source);
    let (group, _, lang) = text::partition(&source, ":");
    let group = group.to_lowercase();
    let sources = graphql(
        http,
        root,
        "get_content_comic_sources",
        GET_CONTENT_COMIC_SOURCES,
        json!({"comicId": manga_id, "dbStatuss": ["normal"], "haveChapter": true}),
    )
    .await?;
    sources
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("data"))
        .find(|data| {
            (group.is_empty() || super::str_of(data, "srcTitle").to_lowercase() == group)
                && (lang.is_empty() || super::str_of(data, "lang") == lang)
        })
        .map(|data| get(data, "id"))
        .ok_or_else(|| error(format!("'{source}' does not match any available source")))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{http, json as reply, keywords as kw, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mangapark.net/title/114972-aria/6710214-en-ch.60.2"),
            Some(Target::Chapter {
                root: "https://mangapark.net".into(),
                id: "6710214".into()
            })
        );
        assert_eq!(
            t("http://www.comicpark.to/comic/10426/aria/c60.2-en-i6712231"),
            Some(Target::Chapter {
                root: "http://www.comicpark.to".into(),
                id: "6712231".into()
            })
        );
        assert!(matches!(
            t("https://mpark.to/comic/1/a/c1-i22-iab"),
            Some(Target::Chapter { ref id, .. }) if id == "22"
        ));
        for raw in [
            "https://mangapark.net/title/114972-aria",
            "https://mpark.to/title/114972-",
            "https://parkmanga.org/title/114972",
            "https://mangapark.io/title/114972/",
            "https://mangapark.net/comic/10426/aria",
        ] {
            assert!(matches!(t(raw), Some(Target::Manga { .. })), "{raw}");
        }
        assert_eq!(
            t("https://mangapark.net/comic/10426/aria"),
            Some(Target::Manga {
                root: "https://mangapark.net".into(),
                id: "10426".into()
            })
        );
        for raw in [
            "https://mangapark.net/title/aria",
            "https://mangapark.net/title/114972-aria?x=1",
            "https://mangapark.net/title/114972x",
            "https://mangapark.net/comic/x/aria/c1-i5",
            "https://mangapark.net/search?word=x",
            "https://mangapark.net.evil.test/title/1-a",
            "https://evilmangapark.net/title/1-a",
            "https://parkmanga.me/title/1-a",
            "https://mangapark.net:8443/title/1-a",
            "https://u:p@mangapark.net/title/1-a",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_chapter_titles() {
        let p = |s: &str| parse_title(s);
        assert_eq!(
            p("Vol.12 Ch.60.2: Special Navigation - Aquaria Ii"),
            (
                12,
                60,
                ".2".into(),
                "Special Navigation - Aquaria Ii".into()
            )
        );
        assert_eq!(p("Chapter 5"), (0, 5, String::new(), String::new()));
        assert_eq!(p("volume 3 CH 7a : x"), (3, 7, "a".into(), "x".into()));
        assert_eq!(p("Ch 3:\nnext\nmore"), (0, 3, String::new(), "next".into()));
        assert_eq!(p("Oneshot"), (0, 0, String::new(), String::new()));
    }

    fn chapter_node(id: &str) -> Value {
        json!({"data": {"get_chapterNode": {"data": {
            "id": id, "dname": "Vol.12 Ch.60.2: Special Navigation", "lang": "en",
            "sourceId": "844", "srcTitle": "Koala", "dateCreate": 1642238703123u64,
            "comicNode": {"id": "114972"},
            "imageFile": {"urlList": [
                "https://s01.mpqom.org/media/2002/e67/a/1_2_3_4.jpeg",
                "https://s01.mpqom.org/media/2002/e67/a/5_6_7_8.jpeg"]}}}}})
    }

    fn comic_node() -> Value {
        json!({"data": {"get_comicNode": {"data": {
            "id": "114972", "name": "Aria", "artists": ["amano kozue"],
            "authors": ["amano kozue"], "genres": ["comedy"]}}}})
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, requests) = serve(vec![reply(chapter_node("6710214")), reply(comic_node())]);
        let http = http("MangaPark", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangapark.io/title/114972-aria/6710214-en-ch.60.2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].headers["Referer"], "https://mangapark.io/");
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Aria");
        assert_eq!(k["manga_id"], 114972);
        assert_eq!(k["artist"], json!(["amano kozue"]));
        assert_eq!(k["genre"], json!(["comedy"]));
        assert_eq!(k["volume"], 12);
        assert_eq!(k["chapter"], 60);
        assert_eq!(k["chapter_minor"], ".2");
        assert_eq!(k["chapter_id"], 6710214);
        assert_eq!(k["title"], "Special Navigation");
        assert_eq!(k["language"], "English");
        assert_eq!(k["source"], "Koala");
        assert_eq!(k["source_id"], "844");
        assert_eq!(k["date"], "2022-01-15 09:25:03");
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "5_6_7_8");
        assert_eq!(
            path(&files[1]),
            "mangapark/Aria/v12 c060.2: Special Navigation/Aria_c060.2_002.jpeg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Aria","60",".2","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("POST /apo/ "));
        assert!(requests[0].contains(r#""operationName":"Get_chapterNode""#));
        assert!(requests[0].contains(r#""variables":{"getChapterNodeId":"6710214"}"#));
        assert!(requests[1].contains(r#""variables":{"getComicNodeId":"114972"}"#));
    }

    #[tokio::test]
    async fn manga_queues_chapters_with_one_comic_lookup() {
        let list = json!({"data": {"get_comicChapterList": [
            {"data": {"id": "1", "dname": "Ch.1", "title": "First", "lang": "en",
                "urlPath": "/title/114972-aria/1-en-ch.1", "srcTitle": "Koala",
                "sourceId": "844", "dateCreate": 0}},
            {"data": {"id": "2", "dname": "Ch.2: Second", "title": null,
                "urlPath": "/title/114972-aria/2-en-ch.2", "srcTitle": "Koala",
                "sourceId": "844", "dateCreate": 0}},
            {"data": {"id": "3", "urlPath": "/elsewhere"}}]}});
        let (root, requests) = serve(vec![
            reply(list),
            reply(chapter_node("1")),
            reply(comic_node()),
            reply(chapter_node("2")),
        ]);
        let http = http("MangaPark", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangapark.net/title/114972-aria").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(kw(&files[0])["subcategory"], "chapter");
        let requests = requests.join().unwrap();
        assert!(requests[0].contains(r#""variables":{"comicId":"114972"}"#));
        assert!(requests[1].contains(r#"{"getChapterNodeId":"1"}"#));
        assert!(requests[2].contains("Get_comicNode"));
        assert!(requests[3].contains(r#"{"getChapterNodeId":"2"}"#));
    }

    #[tokio::test]
    async fn source_option_selects_chapter_list() {
        let sources = json!({"data": {"get_content_comic_sources": [
            {"data": {"id": 1, "srcTitle": "Koala", "lang": "en"}},
            {"data": {"id": 15150116, "srcTitle": "Panda", "lang": "en"}}]}});
        let (root, requests) = serve(vec![
            reply(sources),
            reply(json!({"data": {"get_content_source_chapterList": []}})),
        ]);
        let http = http("MangaPark", root).with_options(Some(std::sync::Arc::new(
            json!({"source": "panda:en"}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://mangapark.net/title/10504-en-mushishi").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].contains(
            r#""variables":{"comicId":10504,"dbStatuss":["normal"],"haveChapter":true}"#
        ));
        assert!(requests[1].contains(r#""variables":{"sourceId":15150116}"#));
    }
}
