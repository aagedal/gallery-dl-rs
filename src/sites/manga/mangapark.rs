//! MangaPark chapters and manga, mirroring gallery_dl/extractor/mangapark.py
//! through the `/apo/` GraphQL API.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{captures, code_to_language, date, group, int_of, parse_int, re, root_of};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

const BASE: &str = r"^https?://(?:www\.)?(?:(?:manga|comic|read)park\.(?:com|net|org|me|io|to)|parkmanga\.(?:com|net|org)|mpark\.to)";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangapark",
    name: "MangaPark",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(&format!(
        r"{BASE}/(?:title/[^/?#]+/|comic/\d+/[^/?#]+/[^/?#]+-i)(\d+)"
    ));
    if let Some(c) = captures(chapter, url) {
        return Some(Target::Chapter(group(&c, 1).into()));
    }
    let manga = re!(&format!(r"{BASE}/(?:title|comic)/(\d+)(?:[/-][^/?#]*)?/?$"));
    captures(manga, url).map(|c| Target::Manga(group(&c, 1).into()))
}

/// `_request_graphql`: the single field of the response's `data`.
async fn graphql(
    http: &Http,
    root: &str,
    operation: &str,
    query: &str,
    variables: Value,
) -> crate::Result<Value> {
    let body = json!({"query": query, "variables": variables, "operationName": operation});
    let data = http
        .post_json(&http.api(&format!("{root}/apo/"))?, &body, &[])
        .await?;
    data["data"]
        .as_object()
        .and_then(|fields| fields.values().next_back().cloned())
        .ok_or_else(|| error("MangaPark returned no GraphQL data"))
}

/// `_parse_chapter_title`: volume, chapter, minor and title.
fn parse_title(title: &str) -> (String, String, String, String) {
    let pattern =
        re!(r"(?i)^(?:vol(?:\.|ume)?\s*(\d+)\s*)?ch(?:\.|apter)?\s*(\d+)([^\s:]*)(?:\s*:\s*(.*))?");
    match pattern.captures(title) {
        Some(c) => (
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
            group(&c, 4).into(),
        ),
        None => ("0".into(), "0".into(), String::new(), String::new()),
    }
}

/// `dateCreate` milliseconds as the `date` keyword.
fn created(value: &Value) -> Value {
    date(value.as_f64().map(|ms| (ms / 1000.0).floor() as i64))
}

fn chapter_keywords(chapter: &Value) -> Map<String, Value> {
    let (vol, ch, minor, title) = parse_title(crate::sites::gdl::str_of(chapter, "dname"));
    let lang = chapter
        .get("lang")
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())
        .unwrap_or("en")
        .to_owned();
    let mut data = Map::new();
    set(&mut data, "volume", parse_int(&vol));
    set(&mut data, "chapter", parse_int(&ch));
    set(&mut data, "chapter_minor", minor);
    set(&mut data, "title", title);
    set(
        &mut data,
        "language",
        code_to_language(&lang).map_or(Value::Null, Value::from),
    );
    set(&mut data, "lang", lang);
    set(&mut data, "source", chapter["srcTitle"].clone());
    set(&mut data, "source_id", chapter["sourceId"].clone());
    set(&mut data, "date", created(&chapter["dateCreate"]));
    data
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaPark URL"));
    };
    let root = root_of(&url);
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(id) => {
                let chapter = graphql(
                    http,
                    &root,
                    "Get_chapterNode",
                    GET_CHAPTERNODE,
                    json!({"getChapterNodeId": id}),
                )
                .await?["data"]
                    .clone();
                let manga_id = crate::sites::scalar(&chapter["comicNode"]["id"]);
                let manga = graphql(
                    http,
                    &root,
                    "Get_comicNode",
                    GET_COMICNODE,
                    json!({"getComicNodeId": manga_id}),
                )
                .await?["data"]
                    .clone();
                let mut data = Map::new();
                set(&mut data, "manga", manga["name"].clone());
                set(&mut data, "manga_id", int_of(manga.get("id")));
                set(&mut data, "artist", manga["artists"].clone());
                set(&mut data, "author", manga["authors"].clone());
                set(&mut data, "genre", manga["genres"].clone());
                data.extend(chapter_keywords(&chapter));
                set(&mut data, "chapter_id", int_of(chapter.get("id")));
                let pages = chapter["imageFile"]["urlList"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|url| (crate::sites::scalar(url), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(id) => {
                let manga_id: i64 = id.parse().unwrap_or(0);
                let mut chapters = match http.option("source") {
                    Some(source) if !source.is_null() => {
                        let source_id = select_source(http, &root, manga_id, source).await?;
                        graphql(
                            http,
                            &root,
                            "get_content_source_chapterList",
                            GET_CONTENT_SOURCE_CHAPTERLIST,
                            json!({"sourceId": source_id}),
                        )
                        .await?
                    }
                    _ => {
                        graphql(
                            http,
                            &root,
                            "Get_comicChapterList",
                            GET_COMICCHAPTERLIST,
                            json!({"comicId": id}),
                        )
                        .await?
                    }
                };
                if http.option_bool("chapter-reverse", false)
                    && let Some(list) = chapters.as_array_mut()
                {
                    list.reverse();
                }
                let children = chapters
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|node| {
                        let chapter = &node["data"];
                        let url = Url::parse(&format!(
                            "{root}{}",
                            crate::sites::scalar(&chapter["urlPath"])
                        ))
                        .ok()?;
                        let mut data = chapter_keywords(chapter);
                        set(&mut data, "manga_id", manga_id);
                        set(&mut data, "chapter_id", chapter["id"].clone());
                        if let Some(title) = chapter["title"].as_str().filter(|t| !t.is_empty()) {
                            set(&mut data, "title", title);
                        }
                        Some((url, data))
                    })
                    .collect();
                super::queue_urls(http, &SITE, &MANGA, children)
            }
        })
    })
}

/// `_select_source`: a source ID, or the first `group[:lang]` match.
async fn select_source(
    http: &Http,
    root: &str,
    manga_id: i64,
    source: &Value,
) -> crate::Result<Value> {
    if source.is_i64() {
        return Ok(source.clone());
    }
    let source = crate::sites::scalar(source);
    let (group, _, lang) = crate::sites::gdl::partition(&source, ":");
    let group = group.to_lowercase();
    let variables = json!({"comicId": manga_id, "dbStatuss": ["normal"], "haveChapter": true});
    let sources = graphql(
        http,
        root,
        "get_content_comic_sources",
        GET_CONTENT_COMIC_SOURCES,
        variables,
    )
    .await?;
    sources
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| &item["data"])
        .find(|data| {
            (group.is_empty()
                || crate::sites::gdl::str_of(data, "srcTitle").to_lowercase() == group)
                && (lang.is_empty() || crate::sites::gdl::str_of(data, "lang") == lang)
        })
        .map(|data| data["id"].clone())
        .ok_or_else(|| error(format!("'{source}' does not match any available source")))
}

const GET_COMICCHAPTERLIST: &str = r#"
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
"#;

const GET_CHAPTERNODE: &str = r#"
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
"#;

const GET_COMICNODE: &str = r#"
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
"#;

const GET_CONTENT_SOURCE_CHAPTERLIST: &str = r#"
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
"#;

const GET_CONTENT_COMIC_SOURCES: &str = r#"
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
"#;

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, http, json as reply, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for raw in [
            "https://mangapark.net/title/114972-aria/6710214-en-ch.60.2",
            "https://comicpark.to/title/114972-aria/6710214-en-ch.60.2",
            "https://parkmanga.com/title/114972-aria/6710214-en-ch.60.2",
            "https://mpark.to/title/114972-aria/6710214-en-ch.60.2",
        ] {
            assert_eq!(t(raw), Some(Target::Chapter("6710214".into())), "{raw}");
        }
        assert_eq!(
            t("https://mangapark.net/comic/10426/aria/c60.2-en-i6712231"),
            Some(Target::Chapter("6712231".into()))
        );
        assert_eq!(
            t("https://mangapark.net/title/114972-aria"),
            Some(Target::Manga("114972".into()))
        );
        assert_eq!(
            t("https://mangapark.net/comic/3227/"),
            Some(Target::Manga("3227".into()))
        );
        assert_eq!(t("https://mangapark.xyz/title/1"), None);
    }

    #[test]
    fn parses_titles() {
        assert_eq!(
            parse_title("Vol.2 Ch.60.2: Navigation"),
            ("2".into(), "60".into(), ".2".into(), "Navigation".into())
        );
        assert_eq!(
            parse_title("Chapter 5"),
            (String::new(), "5".into(), String::new(), String::new())
        );
        assert_eq!(
            parse_title("Oneshot"),
            ("0".into(), "0".into(), String::new(), String::new())
        );
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, server) = serve(vec![
            reply(
                json!({"data": {"get_chapterNode": {"data": {"id": "6710214", "dname": "Ch.60.2 : Navi",
                "lang": "en", "sourceId": 9, "srcTitle": "Src", "dateCreate": 1600000000123i64,
                "comicNode": {"id": "114972"}, "imageFile": {"urlList": ["https://x.mpcdn.org/1.jpg", "https://x.mpcdn.org/2.jpg"]}}}}}),
            ),
            reply(
                json!({"data": {"get_comicNode": {"data": {"id": "114972", "name": "Aria", "artists": ["Amano"], "authors": ["Amano"], "genres": ["sci-fi"]}}}}),
            ),
        ]);
        let http = http("mangapark", root);
        let files = media(extract(
            &http,
            Url::parse("https://mangapark.net/title/114972-aria/6710214-en-ch.60.2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["manga_id"], 114972);
        assert_eq!(k["chapter_id"], 6710214);
        assert_eq!(k["title"], "Navi");
        assert_eq!(k["language"], "English");
        assert_eq!(k["date"], "2020-09-13 12:26:40");
        assert_eq!(
            path(&files[1]),
            crate::sites::testing::os_path("mangapark/Aria/c060.2: Navi/Aria_c060.2_002.jpg")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /apo/ "));
        assert!(requests[0].contains("\"operationName\":\"Get_chapterNode\""));
        assert!(requests[1].contains("\"getComicNodeId\":\"114972\""));
    }

    #[tokio::test]
    async fn manga_queues_chapters_and_selects_sources() {
        let (root, server) = serve(vec![
            reply(json!({"data": {"get_content_comic_sources": [
                {"data": {"id": 1, "srcTitle": "Other", "lang": "en"}},
                {"data": {"id": 2, "srcTitle": "Group", "lang": "en"}}]}})),
            reply(json!({"data": {"get_content_source_chapterList": [
                {"data": {"id": 11, "dname": "Ch.1", "title": "Start", "lang": "en", "urlPath": "/title/1-x/11-en-ch.1",
                    "srcTitle": "Group", "sourceId": 2, "dateCreate": 0}}]}})),
        ]);
        let http = http("mangapark", root).with_options(Some(std::sync::Arc::new(
            json!({"source": "group:en"}).as_object().unwrap().clone(),
        )));
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://mangapark.net/title/1-x").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].contains("\"comicId\":1"));
        assert!(requests[1].contains("\"sourceId\":2"));
    }
}
