//! Reddit's public Atom feeds as API-shaped JSON, for networks where the
//! JSON API sends logged-out clients to its login page while the `.rss`
//! feeds stay open. Each entry becomes a submission with the fields the
//! extractor reads: `id`, `name`, `title`, `author`, `subreddit`,
//! `created_utc`, `permalink` and `url` (the entry's `[link]`), so direct
//! files, `v.redd.it` videos and external links take the usual paths.
//! Feeds carry no gallery metadata or comment trees.
use serde_json::{Map, Value, json};

/// `PATH.json` → `PATH.rss` for listing and submission paths; `None` for
/// other API calls.
pub(crate) fn feed_path(path: &str) -> Option<String> {
    let base = path.strip_suffix(".json")?;
    (!base.contains("api/") && !base.ends_with("about")).then(|| format!("{base}.rss"))
}

/// One feed entry as a submission (`t3`) or comment (`t1`) `data` object.
fn entry(node: roxmltree::Node<'_, '_>) -> Option<(String, Map<String, Value>)> {
    let child = |name: &str| node.children().find(|c| c.has_tag_name(name));
    let text = |name: &str| child(name).and_then(|c| c.text()).map(str::to_owned);
    let full = text("id")?;
    let (kind, id) = full.split_once('_')?;
    if !matches!(kind, "t1" | "t3")
        || id.is_empty()
        || id.len() > 16
        || !id.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    let mut post = Map::new();
    post.insert("id".into(), id.into());
    post.insert("name".into(), full.clone().into());
    if let Some(title) = text("title") {
        post.insert("title".into(), title.into());
    }
    if let Some(author) = child("author")
        .and_then(|a| a.children().find(|c| c.has_tag_name("name")))
        .and_then(|n| n.text())
    {
        let author = author.strip_prefix("/u/").unwrap_or(author);
        post.insert("author".into(), author.into());
    }
    if let Some(term) = node
        .children()
        .find(|c| c.has_tag_name("category"))
        .and_then(|c| c.attribute("term"))
    {
        post.insert("subreddit".into(), term.into());
    }
    if let Some(date) = text("published").or_else(|| text("updated"))
        && let Ok(date) = crate::postprocess::template::parse_date(&date)
    {
        post.insert("created_utc".into(), date.unix_timestamp().into());
    }
    let permalink = child("link")
        .and_then(|l| l.attribute("href"))
        .and_then(|href| url::Url::parse(href).ok());
    if let Some(link) = &permalink {
        post.insert("permalink".into(), link.path().into());
    }
    // The content's `[link]` anchor is the submission URL; self posts link
    // to their own permalink.
    let content = text("content").unwrap_or_default();
    let target = content
        .split("<a href=\"")
        .skip(1)
        .filter_map(|part| {
            let (href, rest) = part.split_once('"')?;
            rest.trim_start_matches('>')
                .starts_with("[link]")
                .then(|| href.replace("&amp;", "&"))
        })
        .next()
        .or_else(|| permalink.as_ref().map(|l| l.to_string()));
    if let Some(target) = target {
        let is_self = permalink
            .as_ref()
            .is_some_and(|l| target.trim_end_matches('/') == l.as_str().trim_end_matches('/'));
        post.insert("is_self".into(), is_self.into());
        post.insert("url".into(), target.into());
    }
    Some((kind.to_owned(), post))
}

fn entries(xml: &str) -> Vec<(String, Map<String, Value>)> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    doc.root_element()
        .children()
        .filter(|c| c.has_tag_name("entry"))
        .filter_map(entry)
        .collect()
}

/// A listing feed as a `Listing`; `after` continues past its last post.
pub(crate) fn listing(xml: &str) -> Option<Value> {
    roxmltree::Document::parse(xml).ok()?;
    let children: Vec<Value> = entries(xml)
        .into_iter()
        .map(|(kind, data)| json!({"kind": kind, "data": data}))
        .collect();
    let after = children
        .last()
        .and_then(|c| c.pointer("/data/name"))
        .cloned()
        .unwrap_or(Value::Null);
    Some(json!({"kind": "Listing", "data": {"children": children, "after": after}}))
}

/// A submission feed (`comments/ID/.rss`) as `[post listing, comments]`.
pub(crate) fn submission(xml: &str, id: &str) -> Option<Value> {
    roxmltree::Document::parse(xml).ok()?;
    let post = entries(xml)
        .into_iter()
        .find(|(kind, data)| kind == "t3" && data.get("id").and_then(Value::as_str) == Some(id))?;
    Some(json!([
        {"kind": "Listing", "data": {"children": [{"kind": "t3", "data": post.1}]}},
        {"kind": "Listing", "data": {"children": []}},
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom">
<entry><author><name>/u/alice</name></author><category term="EarthPorn" label="r/EarthPorn"/>
<content type="html">&lt;a href=&quot;https://www.reddit.com/user/alice&quot;&gt; /u/alice &lt;/a&gt; &lt;span&gt;&lt;a href=&quot;https://i.redd.it/abc.jpeg&quot;&gt;[link]&lt;/a&gt;&lt;/span&gt;</content>
<id>t3_1xdkh9w</id><link href="https://www.reddit.com/r/EarthPorn/comments/1xdkh9w/glow/"/>
<published>2026-10-20T10:00:00+00:00</published><title>Evening glow</title></entry>
<entry><author><name>/u/bob</name></author><category term="EarthPorn"/>
<content type="html">&lt;a href=&quot;https://www.reddit.com/r/EarthPorn/comments/2ab/self/&quot;&gt;[link]&lt;/a&gt;</content>
<id>t3_2ab</id><link href="https://www.reddit.com/r/EarthPorn/comments/2ab/self/"/>
<updated>2026-10-21T10:00:00+00:00</updated><title>Self post</title></entry>
</feed>"#;

    #[test]
    fn feeds_become_api_listings() {
        assert_eq!(
            feed_path("r/pics/top/.json").as_deref(),
            Some("r/pics/top/.rss")
        );
        assert_eq!(feed_path(".json").as_deref(), Some(".rss"));
        assert_eq!(
            feed_path("comments/abc/.json").as_deref(),
            Some("comments/abc/.rss")
        );
        assert_eq!(feed_path("user/bob/about.json"), None);
        assert_eq!(feed_path("api/morechildren"), None);

        let feed = listing(FEED).unwrap();
        let first = &feed["data"]["children"][0]["data"];
        assert_eq!(first["id"], "1xdkh9w");
        assert_eq!(first["author"], "alice");
        assert_eq!(first["subreddit"], "EarthPorn");
        assert_eq!(first["url"], "https://i.redd.it/abc.jpeg");
        assert_eq!(first["is_self"], false);
        assert_eq!(first["created_utc"], 1792490400);
        let second = &feed["data"]["children"][1]["data"];
        assert_eq!(second["is_self"], true);
        assert_eq!(feed["data"]["after"], "t3_2ab");

        let post = submission(FEED, "2ab").unwrap();
        assert_eq!(post[0]["data"]["children"][0]["data"]["title"], "Self post");
        assert!(submission(FEED, "zz").is_none());
        assert!(listing("<html>").is_none());
    }
}
