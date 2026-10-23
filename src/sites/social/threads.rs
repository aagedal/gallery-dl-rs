//! Threads posts (`threads.com/@USER/post/CODE`) through the web client's
//! `BarcelonaPostPageTargetQuery` GraphQL query, which logged-out visitors
//! may run.
//!
//! gallery-dl and yt-dlp have no Threads extractor; formats follow
//! gallery-dl's Instagram conventions, since a Threads post is an Instagram
//! media object (`image_versions2`, `video_versions`, `carousel_media`).
//! The query is persisted under an ID that Meta changes when it redeploys.
//! The last known ID is tried first; if Threads rejects it, the current ID
//! and the query's provided variables are read from the page's own script
//! bundles and the request is repeated once.
use crate::error;
use crate::sites::http::Body;
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "threads",
    name: "Threads",
    matches: |url| post(url).is_some(),
    extract,
    routes: &[&POST],
    api_key: false,
    cookies: Some("threads.com"),
};

static POST: Route = Route::new(
    "post",
    "{post_id}_{num}.{extension}",
    &["{category}", "{username}"],
    "{post_id}_{num}",
);

const ROOT: &str = "https://www.threads.com";
const APP_ID: &str = "238260118697367";
const QUERY: &str = "BarcelonaPostPageTargetQuery";
/// The persisted query ID observed on 2026-10-23.
const KNOWN_DOC_ID: &str = "29589829100635117";
/// Bundles scanned at most when the known ID is stale.
const MAX_BUNDLES: usize = 800;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `(username, shortcode)` of a post URL.
fn post(url: &Url) -> Option<(String, String)> {
    if !crate::sites::plain(url)
        || !matches!(
            url.host_str()?,
            "www.threads.com" | "threads.com" | "www.threads.net" | "threads.net"
        )
    {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let [user, "post", code] = segments.as_slice() else {
        return None;
    };
    let user = user.strip_prefix('@')?;
    let valid_user = !user.is_empty()
        && user.len() <= 30
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_'));
    (valid_user && shortcode_pk(code).is_some()).then(|| (user.to_owned(), (*code).to_owned()))
}

/// The numeric media ID an Instagram-style shortcode encodes.
fn shortcode_pk(code: &str) -> Option<u128> {
    if code.is_empty() || code.len() > 20 {
        return None;
    }
    code.bytes().try_fold(0u128, |pk, c| {
        let digit = ALPHABET.iter().position(|a| *a == c)? as u128;
        pk.checked_mul(64)?.checked_add(digit)
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((user, code)) = post(&url) else {
        return crate::sites::fail(error("unsupported Threads URL"));
    };
    crate::sites::once(async move {
        let pk = shortcode_pk(&code)
            .expect("validated shortcode")
            .to_string();
        let page = http.api(&format!("{ROOT}/@{user}/post/{code}"))?;
        let html = http
            .text_with(
                &page,
                &http.browser_headers(&[("Accept-Language", "en-US,en;q=0.5")]),
            )
            .await?;
        let lsd = crate::sites::extr(&html, r#"["LSD",[],{"token":""#, "\"")
            .filter(|t| !t.is_empty() && t.len() <= 128)
            .ok_or_else(|| error("Threads post page has no session token"))?
            .to_owned();
        let known = Operation {
            doc_id: KNOWN_DOC_ID.into(),
            provided: KNOWN_PROVIDED.iter().map(|s| (*s).to_owned()).collect(),
        };
        let referer = page.to_string();
        let media = match query(http, &known, &pk, &lsd, &referer).await? {
            Some(media) => media,
            None => {
                let current = discover(http, &html).await?;
                if current.doc_id == known.doc_id && current.provided == known.provided {
                    return Err(error("Threads rejected its post query"));
                }
                query(http, &current, &pk, &lsd, &referer)
                    .await?
                    .ok_or_else(|| error("Threads rejected its post query"))?
            }
        };
        if media.get("code").and_then(Value::as_str) != Some(code.as_str()) {
            return Err(error("Threads post not found or not public"));
        }
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &POST,
            items(&media),
        )))
    })
}

#[derive(Debug, PartialEq)]
struct Operation {
    doc_id: String,
    provided: Vec<String>,
}

/// `data.media` of the post query, `None` when Threads rejects the
/// operation (a stale ID or changed provided variables).
async fn query(
    http: &Http,
    operation: &Operation,
    pk: &str,
    lsd: &str,
    referer: &str,
) -> crate::Result<Option<Value>> {
    let mut variables = Map::new();
    variables.insert("postID".into(), pk.into());
    for name in &operation.provided {
        variables.insert(name.clone(), false.into());
    }
    let variables = Value::Object(variables).to_string();
    let endpoint = http.api(&format!("{ROOT}/api/graphql"))?;
    let form = [
        ("lsd", lsd),
        ("variables", variables.as_str()),
        ("doc_id", operation.doc_id.as_str()),
        ("fb_api_req_friendly_name", QUERY),
    ];
    let headers = http.browser_headers(&[
        ("X-FB-LSD", lsd),
        ("X-IG-App-ID", APP_ID),
        ("X-FB-Friendly-Name", QUERY),
        ("Sec-Fetch-Site", "same-origin"),
        ("Origin", ROOT),
        ("Referer", referer),
    ]);
    let body = http.post(&endpoint, Body::Form(&form), &headers).await?;
    // The response may stream several JSON documents; the first has `data`.
    let Some(Ok(data)) = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Value>()
        .next()
    else {
        return Err(error(
            "Threads returned a web page instead of post data; it may be rate limiting \
             logged-out access, so retry later or pass --cookies",
        ));
    };
    Ok(data
        .pointer("/data/media")
        .filter(|media| media.is_object())
        .cloned())
}

/// Read the post query's current ID and provided variables from the page's
/// script bundles.
async fn discover(http: &Http, html: &str) -> crate::Result<Operation> {
    let mut bundles: Vec<Url> = Vec::new();
    let marker = "static.cdninstagram.com";
    let mut rest = html;
    while let Some(pos) = rest.find(marker) {
        let start = rest[..pos].rfind("https:").unwrap_or(pos);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c == '"' || c.is_whitespace())
            .unwrap_or(tail.len());
        let raw = tail[..end].replace("\\/", "/");
        rest = &rest[pos + marker.len()..];
        if let Ok(url) = Url::parse(&raw)
            && url.scheme() == "https"
            && url.host_str() == Some(marker)
            && url.path().starts_with("/rsrc.php/")
            && url.path().ends_with(".js")
            && !bundles.contains(&url)
        {
            bundles.push(url);
        }
    }
    bundles.truncate(MAX_BUNDLES);
    let id_marker =
        format!(r#"__d("{QUERY}_threadsRelayOperation",[],(function(t,n,r,o,a,i){{a.exports=""#);
    let request_marker = format!(r#"name:"{QUERY}",operationKind:"query""#);
    let mut found = Operation {
        doc_id: String::new(),
        provided: Vec::new(),
    };
    let mut scripts = futures_util::stream::iter(bundles)
        .map(|url| async move { http.text(&http.api(url.as_str()).ok()?).await.ok() })
        .buffer_unordered(16);
    while let Some(script) = scripts.next().await {
        let Some(script) = script else { continue };
        if found.doc_id.is_empty()
            && let Some(id) = crate::sites::extr(&script, &id_marker, "\"")
            && !id.is_empty()
            && id.len() <= 32
            && id.bytes().all(|b| b.is_ascii_digit())
        {
            found.doc_id = id.to_owned();
        }
        if found.provided.is_empty()
            && let Some(pos) = script.find(&request_marker)
        {
            let after = &script[pos..];
            if let Some(list) = after
                .find("providedVariables:{")
                .map(|p| &after[p..])
                .and_then(|list| list.find('}').map(|end| &list[..end]))
            {
                let mut names = Vec::new();
                for part in list.split("__relay_internal__pv__").skip(1) {
                    let name: String = part
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        names.push(format!("__relay_internal__pv__{name}"));
                    }
                }
                found.provided = names;
            }
        }
        if !found.doc_id.is_empty() && !found.provided.is_empty() {
            return Ok(found);
        }
    }
    Err(error(
        "Threads changed its post query and the current one could not be found",
    ))
}

fn https(value: Option<&Value>) -> Option<Url> {
    Url::parse(value?.as_str()?)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host_str().is_some())
}

/// The largest candidate of an image or video variant list.
fn largest(list: Option<&Value>) -> Option<Url> {
    list?
        .as_array()?
        .iter()
        .filter(|v| v.get("url").is_some())
        .max_by_key(|v| {
            v.get("width").and_then(Value::as_u64).unwrap_or(0)
                * v.get("height").and_then(Value::as_u64).unwrap_or(0)
        })
        .and_then(|v| https(v.get("url")))
}

fn items(media: &Value) -> Vec<Item> {
    let post_id = media
        .get("pk")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let code = media
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let username = media
        .pointer("/user/username")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let caption = media
        .pointer("/caption/text")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let timestamp = media.get("taken_at").and_then(Value::as_i64);
    let children: Vec<&Value> = match media.get("carousel_media").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().collect(),
        _ => vec![media],
    };
    let mut items = Vec::new();
    for child in children {
        let video = largest(child.get("video_versions"));
        let (url, extension) = match video {
            Some(url) => (url, "mp4"),
            None => match largest(child.pointer("/image_versions2/candidates")) {
                Some(url) => {
                    let ext = if url.path().ends_with(".webp") {
                        "webp"
                    } else {
                        "jpg"
                    };
                    (url, ext)
                }
                None => continue,
            },
        };
        let num = items.len() + 1;
        let media_id = child
            .get("pk")
            .map(crate::sites::scalar)
            .unwrap_or_else(|| format!("{post_id}_{num}"));
        let mut keywords = Map::new();
        keywords.insert("post_id".into(), post_id.clone().into());
        keywords.insert("media_id".into(), media_id.clone().into());
        keywords.insert("shortcode".into(), code.into());
        keywords.insert("username".into(), username.clone().into());
        keywords.insert("description".into(), caption.clone().into());
        keywords.insert("date".into(), timestamp.into());
        keywords.insert("num".into(), num.into());
        for key in ["original_width", "original_height", "like_count"] {
            if let Some(value) = child.get(key).or_else(|| media.get(key)) {
                keywords.insert(key.into(), value.clone());
            }
        }
        keywords.insert("filename".into(), format!("{post_id}_{num}").into());
        keywords.insert("extension".into(), extension.into());
        items.push(
            Item::new(url, keywords)
                .ids(post_id.clone(), media_id)
                .user(username.clone())
                .caption(caption.clone())
                .timestamp(timestamp)
                .header("Referer", &format!("{ROOT}/")),
        );
    }
    items
}

/// The post query's provided variables observed on 2026-10-23.
const KNOWN_PROVIDED: &[&str] = &[
    "__relay_internal__pv__BarcelonaHasCommunityPermalinkPivotsrelayprovider",
    "__relay_internal__pv__BarcelonaHasPodcastV2Consumptionrelayprovider",
    "__relay_internal__pv__BarcelonaHasPodcastV2Productionrelayprovider",
    "__relay_internal__pv__BarcelonaHasInsightsPermalinkUFIrelayprovider",
    "__relay_internal__pv__BarcelonaHasBestOfThreadsrelayprovider",
    "__relay_internal__pv__BarcelonaShouldShowFediverseM1Featuresrelayprovider",
    "__relay_internal__pv__BarcelonaHasDearAlgoConsumptionrelayprovider",
    "__relay_internal__pv__BarcelonaHasMetaAiContentAttachmentsrelayprovider",
    "__relay_internal__pv__BarcelonaHasMWRLocationConsumptionrelayprovider",
    "__relay_internal__pv__BarcelonaShouldFetchPostAuthorFullNamerelayprovider",
    "__relay_internal__pv__BarcelonaIsLoggedInrelayprovider",
    "__relay_internal__pv__BarcelonaMessagesHasLiveChatMessagingrelayprovider",
    "__relay_internal__pv__BarcelonaHasEventBadgerelayprovider",
    "__relay_internal__pv__BarcelonaMessagingHasMetaAIBotrelayprovider",
    "__relay_internal__pv__BarcelonaHasMembersOnlyReplyConsumptionrelayprovider",
    "__relay_internal__pv__BarcelonaGenAIRepliesEnabledrelayprovider",
    "__relay_internal__pv__BarcelonaIsSearchDiscoveryEnabledrelayprovider",
    "__relay_internal__pv__BarcelonaHasCommunitiesrelayprovider",
    "__relay_internal__pv__BarcelonaHasGameScoreSharerelayprovider",
    "__relay_internal__pv__BarcelonaHasPublicViewCountCardrelayprovider",
    "__relay_internal__pv__BarcelonaHasCommunityEmojiUpdateCardrelayprovider",
    "__relay_internal__pv__BarcelonaHasCommunityEntityCardrelayprovider",
    "__relay_internal__pv__BarcelonaHasScorecardCommunityrelayprovider",
    "__relay_internal__pv__BarcelonaHasSportTeamAllegianceCardrelayprovider",
    "__relay_internal__pv__BarcelonaHasStitchedAttachmentrelayprovider",
    "__relay_internal__pv__BarcelonaHasNewspaperLinkStylerelayprovider",
    "__relay_internal__pv__BarcelonaHasMessagingrelayprovider",
    "__relay_internal__pv__BarcelonaOptionalCookiesEnabledrelayprovider",
    "__relay_internal__pv__BarcelonaPostMediaReuseEnabledrelayprovider",
    "__relay_internal__pv__BarcelonaShouldFulfillLightboxQueryrelayprovider",
    "__relay_internal__pv__BarcelonaCanSeeSponsoredContentrelayprovider",
    "__relay_internal__pv__BarcelonaHasUnseenRepliesrelayprovider",
    "__relay_internal__pv__BarcelonaHasWebFaviconsrelayprovider",
    "__relay_internal__pv__BarcelonaIsCrawlerrelayprovider",
    "__relay_internal__pv__BarcelonaHasDearAlgoWebProductionrelayprovider",
    "__relay_internal__pv__BarcelonaHasCommunityTopContributorsrelayprovider",
    "__relay_internal__pv__BarcelonaHasTopLevelSaveButtonrelayprovider",
    "__relay_internal__pv__BarcelonaHasCustomLikesConsumptionrelayprovider",
    "__relay_internal__pv__BarcelonaHasViewerRepliedrelayprovider",
    "__relay_internal__pv__BarcelonaHasPrivateRepliesDeprecationrelayprovider",
    "__relay_internal__pv__BarcelonaHasGhostPostEmojiActivationrelayprovider",
    "__relay_internal__pv__BarcelonaHasGhostPostProductionrelayprovider",
    "__relay_internal__pv__BarcelonaHasNewsletterLinkPreviewrelayprovider",
    "__relay_internal__pv__BarcelonaIsLoggedOutShareTestEnabledrelayprovider",
    "__relay_internal__pv__BarcelonaHasPodcastTranscriptConsumptionrelayprovider",
    "__relay_internal__pv__BarcelonaHasPublicViewCountShareCardrelayprovider",
    "__relay_internal__pv__BarcelonaShouldShowFediverseM075Featuresrelayprovider",
    "__relay_internal__pv__BarcelonaIsInternalUserrelayprovider",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json as reply, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn matches_post_urls() {
        assert_eq!(
            post(&Url::parse("https://www.threads.com/@zuck/post/DeZtp-TkQHh?x=1").unwrap()),
            Some(("zuck".into(), "DeZtp-TkQHh".into()))
        );
        assert_eq!(shortcode_pk("DeZtp-TkQHh"), Some(4006434140014576097));
        for raw in [
            "https://www.threads.com/@zuck",
            "https://www.threads.com/zuck/post/DeZtp",
            "https://www.threads.com/@zuck/post/bad!code",
            "https://evil.test/@zuck/post/DeZtp",
        ] {
            assert_eq!(post(&Url::parse(raw).unwrap()), None, "{raw}");
        }
    }

    fn page(bundle: &str) -> Vec<u8> {
        html(&format!(
            r#"<script>["LSD",[],{{"token":"lsd1"}}]</script><script src="{bundle}"></script>"#
        ))
    }

    fn carousel() -> Value {
        json!({"data": {"media": {"pk": "42", "code": "BA", "taken_at": 1700000000,
            "user": {"username": "zuck"}, "caption": {"text": "hi"},
            "carousel_media": [
                {"pk": "1", "image_versions2": {"candidates": [
                    {"url": "https://cdn.example/s.webp", "width": 1, "height": 1},
                    {"url": "https://cdn.example/l.webp", "width": 9, "height": 9}]}},
                {"pk": "2", "video_versions": [{"url": "https://cdn.example/v.mp4", "width": 5, "height": 5}],
                 "image_versions2": {"candidates": [{"url": "https://cdn.example/poster.jpg"}]}}]}}})
    }

    #[tokio::test]
    async fn extracts_carousels_with_the_known_query() {
        let (root, server) = serve(vec![
            page("https://static.cdninstagram.com/rsrc.php/a.js"),
            reply(carousel()),
        ]);
        let http = http("threads", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.threads.com/@zuck/post/BA").unwrap(),
        ))
        .await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://cdn.example/l.webp", "https://cdn.example/v.mp4"]
        );
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                crate::sites::default_templates(f)
                    .unwrap()
                    .0
                    .render(f)
                    .unwrap()
            })
            .collect();
        assert_eq!(names, ["42_1.webp", "42_2.mp4"]);
        assert_eq!(keywords(&files[0])["username"], "zuck");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /@zuck/post/BA "));
        assert!(requests[1].starts_with("POST /api/graphql "));
        assert!(requests[1].contains(&format!("doc_id={KNOWN_DOC_ID}")));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-fb-lsd: lsd1\r\n")
        );
    }

    #[tokio::test]
    async fn rediscovers_a_rotated_query() {
        let (root, server) = serve(vec![
            page("https://static.cdninstagram.com/rsrc.php/v4/bundle.js"),
            reply(json!({"errors": [{"message": "missing_required_variable_value"}]})),
            html(concat!(
                r#"__d("BarcelonaPostPageTargetQuery_threadsRelayOperation",[],(function(t,n,r,o,a,i){a.exports="777"}),null);"#,
                r#"var e={params:{id:n("x"),name:"BarcelonaPostPageTargetQuery",operationKind:"query",text:null,providedVariables:{__relay_internal__pv__NewFlagrelayprovider:n("y")}}};"#
            )),
            reply(carousel()),
        ]);
        let http = http("threads", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.threads.com/@zuck/post/BA").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[1].contains(&format!("doc_id={KNOWN_DOC_ID}")));
        assert!(
            requests[2].starts_with("GET /rsrc.php/v4/bundle.js "),
            "{}",
            requests[2]
        );
        assert!(requests[3].contains("doc_id=777"), "{}", requests[3]);
        assert!(requests[3].contains("NewFlagrelayprovider"));
        assert!(!requests[3].contains("BarcelonaIsCrawler"));
    }

    #[tokio::test]
    async fn web_pages_are_reported() {
        let (root, _server) = serve(vec![
            page("https://static.cdninstagram.com/rsrc.php/a.js"),
            html("<!DOCTYPE html>"),
        ]);
        let http = http("threads", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://www.threads.com/@zuck/post/BA").unwrap(),
        ))
        .await;
        let error = events[0].as_ref().err().unwrap().to_string();
        assert!(error.contains("rate limiting"), "{error}");
    }
}
