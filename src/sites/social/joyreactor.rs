//! JoyReactor posts, tags, users and searches, mirroring
//! gallery_dl/extractor/joyreactor.py (optional GraphQL `metadata`).
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_int, parse_iso, re,
    rpartition, split_html, strptime, truthy, unescape, unquote, ytdl_item,
};
use crate::sites::{Events, Http, Item, Route, Site};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::time::Duration;
use url::Url;

const FILENAME: &str = "{post_id}_{num:>02}_{file_id}{title:?_//[b:200]}.{extension}";

static POST: Route = Route::new("post", FILENAME, &["{category}"], "{post_id}_{num}");
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "{search_tags}_{post_id}_{num}",
);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "user", "{user}"],
    "{post_id}_{num}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "{search_tags}"],
    "{search_tags}_{post_id}_{num}",
);

pub static SITE: Site = Site {
    category: "joyreactor",
    name: "JoyReactor",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &TAG, &USER, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// Route name, path and name.
    Listing(&'static str, String, String),
}

/// TLD (`m` or `c`) and target.
fn target(url: &Url) -> Option<(String, Target)> {
    const BASE: &str = r"^https?://joyreactor\.c(om|c)";
    if let Some(c) = captures(re!(&format!(r"{BASE}/post/(\d+)")), url) {
        return Some((group(&c, 1).into(), Target::Post(group(&c, 2).into())));
    }
    for kind in ["tag", "user", "search"] {
        let pattern = match kind {
            "tag" => re!(&format!(r"{BASE}(/tag/([^/?#]+)(?:/[^/?#]+)?)")),
            "user" => re!(&format!(r"{BASE}(/user/([^/?#]+)(?:/[^/?#]+)?)")),
            _ => re!(&format!(r"{BASE}(/search/([^/?#]+)(?:/[^/?#]+)?)")),
        };
        if let Some(c) = captures(pattern, url) {
            return Some((
                group(&c, 1).into(),
                Target::Listing(kind, group(&c, 2).into(), group(&c, 3).into()),
            ));
        }
    }
    None
}

/// Upstream `request_interval = (3.0, 6.0)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

async fn graphql(
    http: &Http,
    root: &str,
    operation: &str,
    variables: Value,
) -> crate::Result<Value> {
    let referer = format!("{root}/");
    let body = json!({"variables": variables, "query": query(operation)});
    let data = http
        .post_json(
            &http.api("https://api.joyreactor.com/graphql")?,
            &body,
            &[("Referer", referer.as_str()), ("Origin", root)],
        )
        .await?;
    Ok(data["data"].clone())
}

/// `_extract_files`: images, videos (in the configured formats) and embeds.
fn files(http: &Http, html: &str) -> Vec<Map<String, Value>> {
    let embeds = truthy(http.option("embeds"));
    let videos = http.option("videos").is_none_or(|v| truthy(Some(v)));
    let formats: Option<Vec<String>> = http.option("format").map(|f| match f {
        Value::String(s) => s.split(',').map(str::to_owned).collect(),
        Value::Array(list) => list.iter().map(crate::sites::scalar).collect(),
        other => vec![crate::sites::scalar(other)],
    });
    let mut out = Vec::new();
    let mut last = "";
    for url in extract_iter(html, "src=\"", "\"") {
        if url.contains("/avatar/") || last == url {
            continue;
        }
        last = url;
        let entry = |url: String, kind: &str, format: Option<&str>| {
            let mut file = Map::new();
            file.insert("file_url".into(), url.into());
            file.insert("type".into(), kind.into());
            if let Some(format) = format {
                file.insert("format".into(), format.into());
            }
            file
        };
        if !url.contains(".joyreactor.c") {
            if embeds {
                out.push(entry(format!("ytdl:{url}"), "embed", None));
            }
        } else if url.contains("/webm/") {
            if !videos {
                continue;
            }
            match &formats {
                None => out.push(entry(url.to_owned(), "video", Some("webm"))),
                Some(formats) => {
                    let (lhs, _, rhs) = rpartition(url, "/webm/");
                    let stem = &rhs[..rhs.find('.').unwrap_or(rhs.len())];
                    for format in formats {
                        out.push(entry(
                            format!("{lhs}/{format}/{stem}.{format}"),
                            "video",
                            Some(format),
                        ));
                    }
                }
            }
        } else {
            let (lhs, _, rhs) = rpartition(url, "/");
            out.push(entry(format!("{lhs}/full/{rhs}"), "image", None));
        }
    }
    out
}

/// `JoyreactorExtractor.items` for one post's HTML and GraphQL data.
fn post_items(
    http: &Http,
    root: &str,
    tld: &str,
    html: &str,
    data: Option<&Value>,
    extra: &Map<String, Value>,
) -> Vec<Item> {
    let files = files(http, html);
    let mut scan = Scan::new(html);
    let mut post = Map::new();
    post.insert("count".into(), files.len().into());
    post.insert("user".into(), unescape(scan.extr(" alt=\"", "\"")).into());
    post.insert("user_id".into(), scan.extr("/avatar/user/", "\"").into());
    let tags: Vec<Value> = split_html(scan.extr("class=\"post-tags", "</div>"))
        .into_iter()
        .skip(1)
        .map(Value::from)
        .collect();
    post.insert("tags".into(), tags.into());
    post.insert(
        "content".into(),
        scan.extr("class=\"post-content\">", "<div class=\"post-footer\">")
            .into(),
    );
    let raw_date = scan.extr("</span></button>", "</div>").to_owned();
    post.insert("post_id".into(), scan.extr("\"/post/", "\"").into());
    match data {
        None => {
            let format = if tld == "c" {
                "%d.%m.%y, %H:%M"
            } else {
                "%d/%m/%y, %H:%M"
            };
            post.insert(
                "date".into(),
                date(strptime(rpartition(&raw_date, ">").2, format)),
            );
        }
        Some(data) => {
            post.insert(
                "date".into(),
                date(parse_iso(data["createdAt"].as_str().unwrap_or_default())),
            );
            for (key, source) in [
                ("nsfw", "nsfw"),
                ("unsafe", "unsafe"),
                ("banned", "banned"),
                ("comments", "commentsCount"),
                ("rating", "rating"),
            ] {
                post.insert(key.into(), data.get(source).cloned().unwrap_or(Value::Null));
            }
            if let Some(seo) = data.get("seoAttributes").filter(|s| truthy(Some(s))) {
                post.insert(
                    "title".into(),
                    seo.get("title").cloned().unwrap_or(Value::Null),
                );
            }
        }
    }
    post.extend(extra.clone());
    let referer = format!("{root}/");
    let post_id = crate::sites::scalar(&post["post_id"]);
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let url = crate::sites::scalar(&file["file_url"]);
            post.insert("num".into(), (index + 1).into());
            nameext_into(&url, &mut post);
            post.extend(file);
            let file_id = rpartition(&crate::sites::scalar(&post["filename"]), "-")
                .2
                .to_owned();
            post.insert("file_id".into(), file_id.into());
            let item = match url.strip_prefix("ytdl:") {
                Some(page) => ytdl_item(page, post.clone())?,
                None => Item::new(Url::parse(&url).ok()?, post.clone()),
            };
            Some(
                item.ids(post_id.clone(), (index + 1).to_string())
                    .header("Referer", &referer),
            )
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((tld, target)) = target(&url) else {
        return crate::sites::fail(error("unsupported JoyReactor URL"));
    };
    let root = format!("https://joyreactor.c{tld}");
    let metadata = truthy(http.option("metadata"));
    match target {
        Target::Post(pid) => crate::sites::once(async move {
            pace().await;
            let page = http.text(&http.api(&format!("{root}/post/{pid}"))?).await?;
            let html = extr(&page, "class=\"content\"", "class=\"comment\"");
            let data = if metadata {
                let id = base64::engine::general_purpose::STANDARD.encode(format!("Post:{pid}"));
                Some(
                    graphql(
                        http,
                        &root,
                        "IdPostPageQuery",
                        json!({"id": id, "isAuthorised": false}),
                    )
                    .await?["node"]
                        .clone(),
                )
            } else {
                None
            };
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                post_items(http, &root, &tld, html, data.as_ref(), &Map::new()),
            )))
        }),
        Target::Listing(kind, path, name) => {
            let name = unquote(&name);
            let mut extra = Map::new();
            let (route, operation, variables): (&'static Route, &'static str, Value) = match kind {
                "tag" => {
                    extra.insert("search_tags".into(), name.as_str().into());
                    (
                        &TAG,
                        "TagPageQuery",
                        json!({"name": name, "page": null, "lineType": "GOOD", "isAuthorised": false, "isHomepage": false}),
                    )
                }
                "user" => (
                    &USER,
                    "UserProfilePageQuery",
                    json!({"username": name, "page": null, "lineType": "GOOD", "isAuthorised": false, "isHomepage": false}),
                ),
                _ => {
                    extra.insert("search_tags".into(), name.as_str().into());
                    (
                        &SEARCH,
                        "SearchPageQuery",
                        json!({"query": name, "page": null, "tagNames": [], "username": null, "isAuthorised": false, "showUnsafe": null,
                        "showNsfw": true, "showOnlyNsfw": false, "minRating": null, "maxRating": null, "sortByDate": false, "sortByRating": false}),
                    )
                }
            };
            crate::sites::paginate(
                Some((format!("{root}{path}"), true, variables)),
                move |state| {
                    let root = root.clone();
                    let tld = tld.clone();
                    let extra = extra.clone();
                    async move {
                        let Some((url, first, mut variables)) = state else {
                            return Ok(None);
                        };
                        pace().await;
                        let page = http.text(&http.api(&url)?).await?;
                        let mut posts: Vec<&str> =
                            page.split("class=\"content\"").skip(1).collect();
                        if first
                            && posts.first().is_some_and(|p| {
                                p.contains(
                                    "class=\"expand-wrapper relative\"><div class=\"absolute",
                                )
                            })
                        {
                            posts.remove(0);
                        }
                        let path =
                            extr(extr(&page, "ant-pagination-next", "</li>"), "href=\"", "\"")
                                .to_owned();
                        let data_posts: Vec<Value> = if metadata {
                            variables["page"] = if path.is_empty() {
                                Value::Null
                            } else {
                                (parse_int(&path[path.rfind('/').map_or(0, |i| i + 1)..]) + 1)
                                    .into()
                            };
                            let data = graphql(http, &root, operation, variables.clone()).await?;
                            data.as_object()
                                .and_then(|m| m.values().next_back())
                                .and_then(|v| v["postPager"]["posts"].as_array().cloned())
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        let events = posts
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| !metadata || *index < data_posts.len())
                            .flat_map(|(index, html)| {
                                crate::sites::post(
                                    &SITE,
                                    http,
                                    route,
                                    post_items(
                                        http,
                                        &root,
                                        &tld,
                                        html,
                                        data_posts.get(index),
                                        &extra,
                                    ),
                                )
                            })
                            .collect();
                        // Upstream continues only while the next link ends in `/1/rev`.
                        let next = (!path.is_empty() && path.ends_with("/1/rev"))
                            .then(|| (format!("{root}{path}"), false, variables));
                        Ok(Some((crate::sites::events(events), next)))
                    }
                },
            )
        }
    }
}

fn query(operation: &str) -> &'static str {
    match operation {
        "IdPostPageQuery" => ID_POST_PAGE_QUERY,
        "TagPageQuery" => TAG_PAGE_QUERY,
        "UserProfilePageQuery" => USER_PROFILE_PAGE_QUERY,
        "SearchPageQuery" => SEARCH_PAGE_QUERY,
        _ => "",
    }
}

const ID_POST_PAGE_QUERY: &str = r#"query IdPostPageQuery(
  $id: ID!
  $isAuthorised: Boolean!
) {
  node(id: $id) {
    __typename
    ... on Post {
      id
      user {
        id
      }
      text
      nsfw
      unsafe
      seoAttributes {
        title
        description
        ocr
        similarPosts
      }
      ...Post_post_2lIf9C
      comments {
        ...CommentTree_comments_48QSE5
        id
      }
      tags {
        ...TagList_blogs
        id
      }
    }
    id
  }
}

fragment AttributeEmbed_attribute on AttributeEmbed {
  __isAttributeEmbed: __typename
  type
  value
  image {
    comment
    id
  }
}

fragment AttributePicture_attribute on AttributePicture {
  __isAttributePicture: __typename
  id
  type
  insertId
  image {
    width
    height
    type
    comment
    hasVideo
    id
  }
}

fragment AttributePicture_post on Post {
  nsfw
  tags {
    name
    seoName
    synonyms
    id
  }
}

fragment Attribute_attribute on Attribute {
  __isAttribute: __typename
  type
  ...AttributePicture_attribute
  ...AttributeEmbed_attribute
}

fragment Attribute_post on Post {
  ...AttributePicture_post
}

fragment CommentTree_comments_2EWd0p on Comment {
  id
  locale
  level
  parent {
    __typename
    id
  }
  createdAt
  user {
    id
  }
  ...Comment_comment_2EWd0p
}

fragment CommentTree_comments_48QSE5 on Comment {
  id
  locale
  level
  parent {
    __typename
    id
  }
  createdAt
  user {
    id
  }
  ...Comment_comment_48QSE5
}

fragment CommentTree_post on Post {
  id
  ...Comment_post
}

fragment CommentVote_comment on Comment {
  id
  rating
  voted
}

fragment Comment_comment_2EWd0p on Comment {
  id
  user {
    id
    username
  }
  createdAt
  rating
  level
  contentVersion
  banned
  contentEditedAt
  locale
  ...EditableCommentContent_content
  ...Content_content
}

fragment Comment_comment_48QSE5 on Comment {
  id
  user {
    id
    username
  }
  createdAt
  rating
  level
  contentVersion
  banned
  contentEditedAt
  locale
  ...EditableCommentContent_content
  ...Content_content
  ...CommentVote_comment @include(if: $isAuthorised)
}

fragment Comment_post on Post {
  id
  ...Content_post
}

fragment Content_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    ...Attribute_attribute
  }
}

fragment Content_post on Post {
  ...Attribute_post
}

fragment EditableCommentContent_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    type
    image {
      id
      hasVideo
      type
      width
      height
    }
    ... on CommentAttributeEmbed {
      value
    }
  }
}

fragment Poll_post_2lIf9C on Post {
  id
  poll {
    question
    answers {
      id
      answer
      count
    }
    voted @include(if: $isAuthorised)
  }
}

fragment PostComments_post_2lIf9C on Post {
  id
  viewedCommentsAt @include(if: $isAuthorised)
  viewedCommentsCount @include(if: $isAuthorised)
  commentsCount
  user {
    id
  }
  unsafe
  ...CommentTree_post
}

fragment PostFooter_post_2lIf9C on Post {
  id
  commentsCount
  rating
  ratingGeneral
  createdAt
  viewedCommentsCount @include(if: $isAuthorised)
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
}

fragment PostTags_post on Post {
  id
  user {
    id
  }
  postTags {
    deletable
    tag {
      id
      name
      seoName
      showAsCategory
      mainTag {
        id
        category {
          id
        }
        userTag {
          state
        }
      }
    }
  }
}

fragment PostVote_post on Post {
  id
  rating
  ratingGeneral
  minusThreshold
  vote {
    createdAt
    power
  }
}

fragment Post_post_2lIf9C on Post {
  id
  user {
    id
    username
  }
  bestComments {
    ...CommentTree_comments_2EWd0p
    id
  }
  tags {
    mainTag {
      id
      name
      category {
        id
      }
      userTag {
        state
      }
    }
    id
  }
  nsfw
  unsafe
  createdAt
  editableUntil
  text
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
  banned
  poll {
    question
  }
  commentsCount
  ...CommentTree_post
  ...PostTags_post
  ...Content_post
  ...Content_content
  ...PostFooter_post_2lIf9C
  ...PostComments_post_2lIf9C
  ...Poll_post_2lIf9C
}

fragment TagList_blogs on Tag {
  id
  name
  seoName
  count
  subscribers
  showAsCategory
  mainTag {
    nsfw
    unsafe
    category {
      name
      id
    }
    id
  }
}
"#;

const TAG_PAGE_QUERY: &str = r#"query TagPageQuery(
  $name: String
  $lineType: PostLineType!
  $favoriteType: PostLineType
  $page: Int
  $isAuthorised: Boolean!
  $isHomepage: Boolean!
) {
  tag(name: $name) {
    id
    name
    mainTag {
      id
      hierarchy {
        mainTag {
          name
          id
        }
        id
      }
      nsfw
      unsafe
      synonyms
      count
      seoName
      category {
        name
        unsafe
        nsfw
        id
      }
    }
    count
    postPager(type: $lineType, favoriteType: $favoriteType) {
      count
      id
    }
    ...TagHeader_blog @skip(if: $isHomepage)
    ...TagSidebar_blog @skip(if: $isHomepage)
    ...TagPostPager_blog_qTg8U
  }
}

fragment AttributeEmbed_attribute on AttributeEmbed {
  __isAttributeEmbed: __typename
  type
  value
  image {
    comment
    id
  }
}

fragment AttributePicture_attribute on AttributePicture {
  __isAttributePicture: __typename
  id
  type
  insertId
  image {
    width
    height
    type
    comment
    hasVideo
    id
  }
}

fragment AttributePicture_post on Post {
  nsfw
  tags {
    name
    seoName
    synonyms
    id
  }
}

fragment Attribute_attribute on Attribute {
  __isAttribute: __typename
  type
  ...AttributePicture_attribute
  ...AttributeEmbed_attribute
}

fragment Attribute_post on Post {
  ...AttributePicture_post
}

fragment BlogDescription_blog on Tag {
  id
  articlePost {
    ...Content_post
    ...Content_content
    id
  }
}

fragment CommentTree_comments_2EWd0p on Comment {
  id
  locale
  level
  parent {
    __typename
    id
  }
  createdAt
  user {
    id
  }
  ...Comment_comment_2EWd0p
}

fragment CommentTree_post on Post {
  id
  ...Comment_post
}

fragment Comment_comment_2EWd0p on Comment {
  id
  user {
    id
    username
  }
  createdAt
  rating
  level
  contentVersion
  banned
  contentEditedAt
  locale
  ...EditableCommentContent_content
  ...Content_content
}

fragment Comment_post on Post {
  id
  ...Content_post
}

fragment Content_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    ...Attribute_attribute
  }
}

fragment Content_post on Post {
  ...Attribute_post
}

fragment EditableCommentContent_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    type
    image {
      id
      hasVideo
      type
      width
      height
    }
    ... on CommentAttributeEmbed {
      value
    }
  }
}

fragment Poll_post_2lIf9C on Post {
  id
  poll {
    question
    answers {
      id
      answer
      count
    }
    voted @include(if: $isAuthorised)
  }
}

fragment PostComments_post_2lIf9C on Post {
  id
  viewedCommentsAt @include(if: $isAuthorised)
  viewedCommentsCount @include(if: $isAuthorised)
  commentsCount
  user {
    id
  }
  unsafe
  ...CommentTree_post
}

fragment PostFooter_post_2lIf9C on Post {
  id
  commentsCount
  rating
  ratingGeneral
  createdAt
  viewedCommentsCount @include(if: $isAuthorised)
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
}

fragment PostPager_posts_3OSKdM on PostPager {
  posts(page: $page) {
    id
    nsfw
    unsafe
    tags {
      mainTag {
        id
        nsfw
        unsafe
        category {
          id
        }
        userTag {
          state
        }
      }
      id
    }
    user {
      username
      id
    }
    commentsCount
    ...Post_post_2lIf9C
  }
  count
  id
}

fragment PostTags_post on Post {
  id
  user {
    id
  }
  postTags {
    deletable
    tag {
      id
      name
      seoName
      showAsCategory
      mainTag {
        id
        category {
          id
        }
        userTag {
          state
        }
      }
    }
  }
}

fragment PostVote_post on Post {
  id
  rating
  ratingGeneral
  minusThreshold
  vote {
    createdAt
    power
  }
}

fragment Post_post_2lIf9C on Post {
  id
  user {
    id
    username
  }
  bestComments {
    ...CommentTree_comments_2EWd0p
    id
  }
  tags {
    mainTag {
      id
      name
      category {
        id
      }
      userTag {
        state
      }
    }
    id
  }
  nsfw
  unsafe
  createdAt
  editableUntil
  text
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
  banned
  poll {
    question
  }
  commentsCount
  ...CommentTree_post
  ...PostTags_post
  ...Content_post
  ...Content_content
  ...PostFooter_post_2lIf9C
  ...PostComments_post_2lIf9C
  ...Poll_post_2lIf9C
}

fragment TagHeader_blog on Tag {
  id
  seoName
  name
  mainTag {
    id
    unsafe
    nsfw
    articlePost {
      id
    }
    ...BlogDescription_blog
    subTagsMenu {
      ...TagList_blogs
      id
    }
    subTags {
      ...TagList_blogs
      id
    }
    ...TagSuperBlogs_blog
    hierarchy {
      mainTag {
        id
        name
        showAsCategory
      }
      id
    }
    synonyms
    subscribers
    count
    image {
      id
    }
    userTag {
      state
    }
    articleImage {
      id
      type
    }
    category {
      id
      name
      category {
        id
      }
      showAsCategory
      nsfw
      unsafe
    }
    moderators {
      ...UserList_users
      id
    }
  }
  ...TagSidebar_blog
}

fragment TagList_blogs on Tag {
  id
  name
  seoName
  count
  subscribers
  showAsCategory
  mainTag {
    nsfw
    unsafe
    category {
      name
      id
    }
    id
  }
}

fragment TagPostPager_blog_qTg8U on Tag {
  unsafe
  postPager(type: $lineType, favoriteType: $favoriteType) {
    ...PostPager_posts_3OSKdM
    count
    id
  }
}

fragment TagSidebar_blog on Tag {
  name
  mainTag {
    subTagsMenu {
      id
    }
    subTags {
      ...TagList_blogs
      id
    }
    ...TagSuperBlogs_blog
    nsfw
    unsafe
    category {
      id
      name
      category {
        id
      }
      nsfw
      unsafe
    }
    moderators {
      ...UserList_users
      id
    }
    id
  }
}

fragment TagSuperBlogs_blog on Tag {
  subTagsMenu {
    id
    name
    nsfw
    unsafe
    showAsCategory
  }
}

fragment UserList_users on User {
  id
  username
}
"#;

const USER_PROFILE_PAGE_QUERY: &str = r#"query UserProfilePageQuery(
  $username: String!
  $page: Int
  $isAuthorised: Boolean!
  $selfOrAdmin: Boolean!
) {
  user(username: $username) {
    id
    postPager {
      ...PostPager_posts_3OSKdM
      count
      id
    }
    ...UserSidebar_user_2lIf9C
    active @include(if: $selfOrAdmin)
    postsBannedUntil @include(if: $selfOrAdmin)
    commentsBannedUntil @include(if: $selfOrAdmin)
    tagBans @include(if: $selfOrAdmin) {
      tag {
        id
        name
      }
      bannedUntil
    }
    donatedLeft @include(if: $selfOrAdmin)
    goldStatusExpire @include(if: $selfOrAdmin)
    platinumStatusExpire @include(if: $selfOrAdmin)
    settings @include(if: $selfOrAdmin) {
      goldStatusExtend
    }
    userState @include(if: $isAuthorised)
  }
  me {
    goldStatus
    platinumStatus
    id
  }
}

fragment AttributeEmbed_attribute on AttributeEmbed {
  __isAttributeEmbed: __typename
  type
  value
  image {
    comment
    id
  }
}

fragment AttributePicture_attribute on AttributePicture {
  __isAttributePicture: __typename
  id
  type
  insertId
  image {
    width
    height
    type
    comment
    hasVideo
    id
  }
}

fragment AttributePicture_post on Post {
  nsfw
  tags {
    name
    seoName
    synonyms
    id
  }
}

fragment Attribute_attribute on Attribute {
  __isAttribute: __typename
  type
  ...AttributePicture_attribute
  ...AttributeEmbed_attribute
}

fragment Attribute_post on Post {
  ...AttributePicture_post
}

fragment CommentTree_comments_2EWd0p on Comment {
  id
  locale
  level
  parent {
    __typename
    id
  }
  createdAt
  user {
    id
  }
  ...Comment_comment_2EWd0p
}

fragment CommentTree_post on Post {
  id
  ...Comment_post
}

fragment Comment_comment_2EWd0p on Comment {
  id
  user {
    id
    username
  }
  createdAt
  rating
  level
  contentVersion
  banned
  contentEditedAt
  locale
  ...EditableCommentContent_content
  ...Content_content
}

fragment Comment_post on Post {
  id
  ...Content_post
}

fragment Content_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    ...Attribute_attribute
  }
}

fragment Content_post on Post {
  ...Attribute_post
}

fragment EditableCommentContent_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    type
    image {
      id
      hasVideo
      type
      width
      height
    }
    ... on CommentAttributeEmbed {
      value
    }
  }
}

fragment Poll_post_2lIf9C on Post {
  id
  poll {
    question
    answers {
      id
      answer
      count
    }
    voted @include(if: $isAuthorised)
  }
}

fragment PostComments_post_2lIf9C on Post {
  id
  viewedCommentsAt @include(if: $isAuthorised)
  viewedCommentsCount @include(if: $isAuthorised)
  commentsCount
  user {
    id
  }
  unsafe
  ...CommentTree_post
}

fragment PostFooter_post_2lIf9C on Post {
  id
  commentsCount
  rating
  ratingGeneral
  createdAt
  viewedCommentsCount @include(if: $isAuthorised)
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
}

fragment PostPager_posts_3OSKdM on PostPager {
  posts(page: $page) {
    id
    nsfw
    unsafe
    tags {
      mainTag {
        id
        nsfw
        unsafe
        category {
          id
        }
        userTag {
          state
        }
      }
      id
    }
    user {
      username
      id
    }
    commentsCount
    ...Post_post_2lIf9C
  }
  count
  id
}

fragment PostTags_post on Post {
  id
  user {
    id
  }
  postTags {
    deletable
    tag {
      id
      name
      seoName
      showAsCategory
      mainTag {
        id
        category {
          id
        }
        userTag {
          state
        }
      }
    }
  }
}

fragment PostVote_post on Post {
  id
  rating
  ratingGeneral
  minusThreshold
  vote {
    createdAt
    power
  }
}

fragment Post_post_2lIf9C on Post {
  id
  user {
    id
    username
  }
  bestComments {
    ...CommentTree_comments_2EWd0p
    id
  }
  tags {
    mainTag {
      id
      name
      category {
        id
      }
      userTag {
        state
      }
    }
    id
  }
  nsfw
  unsafe
  createdAt
  editableUntil
  text
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
  banned
  poll {
    question
  }
  commentsCount
  ...CommentTree_post
  ...PostTags_post
  ...Content_post
  ...Content_content
  ...PostFooter_post_2lIf9C
  ...PostComments_post_2lIf9C
  ...Poll_post_2lIf9C
}

fragment TagList_blogs on Tag {
  id
  name
  seoName
  count
  subscribers
  showAsCategory
  mainTag {
    nsfw
    unsafe
    category {
      name
      id
    }
    id
  }
}

fragment UserAwards_awardUser on AwardUser {
  award {
    id
    name
    description
    picUrl
    nextAward {
      id
    }
  }
  hidden
}

fragment UserList_users on User {
  id
  username
}

fragment UserSidebar_user_2lIf9C on User {
  id
  username
  rating
  ratingWeek
  hideSubscriptionsRatings
  commentNum
  postNum
  goodPostNum
  bestPostNum
  about
  createdAt
  sequentialVisits
  totalVisits
  lastVisit
  canSendPrivateMessage @include(if: $isAuthorised)
  awards {
    ...UserAwards_awardUser
  }
  subscribedTags {
    ...TagList_blogs
    id
  }
  moderatedTags {
    ...TagList_blogs
    id
  }
  blockedTags {
    ...TagList_blogs
    id
  }
  friends {
    ...UserList_users
    id
  }
  blockedUsers {
    ...UserList_users
    id
  }
  topTagRatings {
    ...UserTagRatings_favoriteTag
  }
}

fragment UserTagRatings_favoriteTag on UserTag {
  tag {
    id
    name
    nsfw
    unsafe
    showAsCategory
  }
  rating
}
"#;

const SEARCH_PAGE_QUERY: &str = r#"query SearchPageQuery(
  $query: String!
  $showNsfw: Boolean
  $showUnsafe: Boolean
  $showOnlyNsfw: Boolean
  $tagNames: [String!]
  $username: String
  $page: Int
  $isAuthorised: Boolean!
  $minRating: Int
  $maxRating: Int
  $sortByRating: Boolean
  $sortByDate: Boolean
) {
  search(query: $query, showNsfw: $showNsfw, showUnsafe: $showUnsafe,   showOnlyNsfw: $showOnlyNsfw, username: $username, tagNames: $tagNames,   sortByDate: $sortByDate, sortByRating: $sortByRating, minRating:   $minRating, maxRating: $maxRating) {
    tags {
      ...TagList_blogs
      mainTag {
        id
        unsafe
      }
      id
    }
    postPager {
      ...PostPager_posts_3OSKdM
      count
      id
    }
    similarQueries @skip(if: $isAuthorised)
  }
}

fragment AttributeEmbed_attribute on AttributeEmbed {
  __isAttributeEmbed: __typename
  type
  value
  image {
    comment
    id
  }
}

fragment AttributePicture_attribute on AttributePicture {
  __isAttributePicture: __typename
  id
  type
  insertId
  image {
    width
    height
    type
    comment
    hasVideo
    id
  }
}

fragment AttributePicture_post on Post {
  nsfw
  tags {
    name
    seoName
    synonyms
    id
  }
}

fragment Attribute_attribute on Attribute {
  __isAttribute: __typename
  type
  ...AttributePicture_attribute
  ...AttributeEmbed_attribute
}

fragment Attribute_post on Post {
  ...AttributePicture_post
}

fragment CommentTree_comments_2EWd0p on Comment {
  id
  locale
  level
  parent {
    __typename
    id
  }
  createdAt
  user {
    id
  }
  ...Comment_comment_2EWd0p
}

fragment CommentTree_post on Post {
  id
  ...Comment_post
}

fragment Comment_comment_2EWd0p on Comment {
  id
  user {
    id
    username
  }
  createdAt
  rating
  level
  contentVersion
  banned
  contentEditedAt
  locale
  ...EditableCommentContent_content
  ...Content_content
}

fragment Comment_post on Post {
  id
  ...Content_post
}

fragment Content_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    ...Attribute_attribute
  }
}

fragment Content_post on Post {
  ...Attribute_post
}

fragment EditableCommentContent_content on Content {
  __isContent: __typename
  text
  attributes {
    __typename
    id
    insertId
    type
    image {
      id
      hasVideo
      type
      width
      height
    }
    ... on CommentAttributeEmbed {
      value
    }
  }
}

fragment Poll_post_2lIf9C on Post {
  id
  poll {
    question
    answers {
      id
      answer
      count
    }
    voted @include(if: $isAuthorised)
  }
}

fragment PostComments_post_2lIf9C on Post {
  id
  viewedCommentsAt @include(if: $isAuthorised)
  viewedCommentsCount @include(if: $isAuthorised)
  commentsCount
  user {
    id
  }
  unsafe
  ...CommentTree_post
}

fragment PostFooter_post_2lIf9C on Post {
  id
  commentsCount
  rating
  ratingGeneral
  createdAt
  viewedCommentsCount @include(if: $isAuthorised)
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
}

fragment PostPager_posts_3OSKdM on PostPager {
  posts(page: $page) {
    id
    nsfw
    unsafe
    tags {
      mainTag {
        id
        nsfw
        unsafe
        category {
          id
        }
        userTag {
          state
        }
      }
      id
    }
    user {
      username
      id
    }
    commentsCount
    ...Post_post_2lIf9C
  }
  count
  id
}

fragment PostTags_post on Post {
  id
  user {
    id
  }
  postTags {
    deletable
    tag {
      id
      name
      seoName
      showAsCategory
      mainTag {
        id
        category {
          id
        }
        userTag {
          state
        }
      }
    }
  }
}

fragment PostVote_post on Post {
  id
  rating
  ratingGeneral
  minusThreshold
  vote {
    createdAt
    power
  }
}

fragment Post_post_2lIf9C on Post {
  id
  user {
    id
    username
  }
  bestComments {
    ...CommentTree_comments_2EWd0p
    id
  }
  tags {
    mainTag {
      id
      name
      category {
        id
      }
      userTag {
        state
      }
    }
    id
  }
  nsfw
  unsafe
  createdAt
  editableUntil
  text
  favorite @include(if: $isAuthorised)
  ...PostVote_post @include(if: $isAuthorised)
  banned
  poll {
    question
  }
  commentsCount
  ...CommentTree_post
  ...PostTags_post
  ...Content_post
  ...Content_content
  ...PostFooter_post_2lIf9C
  ...PostComments_post_2lIf9C
  ...Poll_post_2lIf9C
}

fragment TagList_blogs on Tag {
  id
  name
  seoName
  count
  subscribers
  showAsCategory
  mainTag {
    nsfw
    unsafe
    category {
      name
      id
    }
    id
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("http://joyreactor.com/post/3721876"),
            Some(("om".into(), Target::Post("3721876".into())))
        );
        assert_eq!(
            t("http://joyreactor.cc/tag/Advent+Cirno"),
            Some((
                "c".into(),
                Target::Listing("tag", "/tag/Advent+Cirno".into(), "Advent+Cirno".into())
            ))
        );
        assert_eq!(
            t("http://joyreactor.com/user/hemantic/2"),
            Some((
                "om".into(),
                Target::Listing("user", "/user/hemantic/2".into(), "hemantic".into())
            ))
        );
    }

    #[tokio::test]
    async fn post_page_files() {
        let page = r#"<div class="content"><img alt="jane" src="https://img.joyreactor.com/avatar/user/77"> <div class="post-tags"><a>Tags</a><a>art</a></div>
            <div class="post-content"><img src="https://img10.joyreactor.com/pics/post/art-cat-12345.jpeg"><img src="https://img10.joyreactor.com/pics/post/art-cat-12345.jpeg">
            <video src="https://img10.joyreactor.com/pics/post/webm/cat-555.webm"></video><div class="post-footer">
            <span></span></button><span>x</span>05/01/24, 13:45</div><a href="/post/3721876">link</a><div class="comment">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("joyreactor", root);
        let files = media(extract(
            &http,
            Url::parse("http://joyreactor.com/post/3721876").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://img10.joyreactor.com/pics/post/full/art-cat-12345.jpeg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://img10.joyreactor.com/pics/post/webm/cat-555.webm"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["user"].as_str(),
                k["file_id"].as_str(),
                k["date"].as_str()
            ),
            (Some("jane"), Some("12345"), Some("2024-01-05 13:45:00"))
        );
        let (file, _) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "3721876_02_555.webm");
        server.join().unwrap();
    }
}
