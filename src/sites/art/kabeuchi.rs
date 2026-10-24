//! Kabe-uchiroom user uploads, mirroring gallery_dl/extractor/kabeuchi.py:
//! the profile page names the user's friend id, whose `get_posts.php` pages
//! list up to six image file names per post.
use super::{extr, leading_digits, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://kabe-uchiroom.com";
static USER: Route = Route::new(
    "user",
    "{id}_{num:>02}{title:?_//}.{extension}",
    &["{category}", "{twitter_user_id} {twitter_id}"],
    "{id}_{num}",
);

pub static SITE: Site = Site {
    category: "kabeuchi",
    name: "Kabe-uchiroom",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER],
    api_key: false,
    cookies: None,
};

/// `kabe-uchiroom\.com/mypage/?\?id=(\d+)`, without `www.`.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str()? != "kabe-uchiroom.com" {
        return None;
    }
    if !matches!(url.path(), "/mypage" | "/mypage/") {
        return None;
    }
    let id = leading_digits(url.query()?.strip_prefix("id=")?)?;
    Some(id.to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(uid) = target(&url) else {
        return crate::sites::fail(error("unsupported Kabe-uchiroom URL"));
    };
    crate::sites::once(async move {
        let (location, page) = http
            .page(&http.api(&format!("{ROOT}/mypage/?id={uid}"))?, &[])
            .await?;
        // A missing user redirects to the front page.
        if location.path() == "/" {
            return Err(error("Kabe-uchiroom user not found"));
        }
        let target_id = extr(&page, "user_friend_id = \"", "\"").to_owned();
        Ok(paginate(http, uid, target_id))
    })
}

/// Upstream `_pagination`: request pages until the last post id equals the
/// response's `last_data`.
fn paginate(http: &Http, uid: String, target_id: String) -> Events<'_> {
    crate::sites::paginate(Some((0u64, String::new())), move |state| {
        let (uid, target_id) = (uid.clone(), target_id.clone());
        async move {
            let Some((page, latest)) = state else {
                return Ok(None);
            };
            let url = http.api(&format!("{ROOT}/get_posts.php"))?;
            let page_num = page.to_string();
            let form = [
                ("user_id", "0"),
                ("target_id", target_id.as_str()),
                ("type", "uploads"),
                ("sort_type", "0"),
                ("category_id", "all"),
                ("latest_post", latest.as_str()),
                ("page_num", page_num.as_str()),
            ];
            let info = http.post_form(&url, &form, &[]).await?;
            let Some(posts) = info.get("datas").and_then(Value::as_array) else {
                return Ok(None);
            };
            let Some(last) = posts.last() else {
                return Ok(None);
            };
            let last_id = last.get("id").cloned().unwrap_or(Value::Null);
            let next = (last_id != info.get("last_data").cloned().unwrap_or(Value::Null))
                .then(|| (page + 1, crate::sites::scalar(&last_id)));
            let events = posts
                .iter()
                .flat_map(|post| {
                    crate::sites::post(
                        &SITE,
                        http,
                        &USER,
                        parse_post(&uid, post).unwrap_or_default(),
                    )
                })
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// One post's files; ads and posts without images yield none.
pub(crate) fn parse_post(uid: &str, post: &Value) -> Result<Vec<Item>> {
    let mut data: Map<String, Value> = post
        .as_object()
        .cloned()
        .ok_or_else(|| error("invalid Kabe-uchiroom post"))?;
    if truthy(post.get("is_ad")) || !truthy(post.get("image1")) {
        return Ok(Vec::new());
    }
    let timestamp = super::parse_iso(str_of(post, "created_at"));
    data.insert("date".into(), super::date(timestamp));
    let last = uid.chars().last().unwrap_or_default();
    let base = format!("{ROOT}/accounts/upfile/{last}/{uid}/");
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let title = str_of(post, "title").to_owned();
    let mut items = Vec::new();
    for num in 1..=6 {
        let name = str_of(post, &format!("image{num}"));
        if name.is_empty() {
            break;
        }
        let mut fields = data.clone();
        fields.insert("num".into(), num.into());
        super::nameext_into(name, &mut fields);
        let Some(url) = super::file_url(name, &base) else {
            continue;
        };
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), num.to_string())
                .caption(Some(title.clone()))
                .timestamp(timestamp),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://kabe-uchiroom.com/mypage/?id=919865303848255493"),
            Some("919865303848255493".into())
        );
        assert_eq!(
            t("http://kabe-uchiroom.com/mypage?id=12x"),
            Some("12".into())
        );
        for raw in [
            "https://kabe-uchiroom.com/mypage/?x=1&id=1",
            "https://kabe-uchiroom.com/mypage/?id=x",
            "https://kabe-uchiroom.com/other/?id=1",
            "https://www.kabe-uchiroom.com/mypage/?id=1",
            "https://kabe-uchiroom.com.evil.com/mypage/?id=1",
            "https://kabe-uchiroom.com:8080/mypage/?id=1",
            "https://u:p@kabe-uchiroom.com/mypage/?id=1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_uploads() {
        let profile = "<script>var user_friend_id = \"77\";</script>";
        let first = json!({"last_data": 1, "datas": [
            {"id": 3, "title": "Hello", "twitter_user_id": "tw", "twitter_id": 5,
             "created_at": "2020-01-02 03:04:05", "image1": "a.jpg", "image2": "b.PNG",
             "image3": "", "image4": "x.jpg"},
            {"id": 2, "is_ad": 1, "image1": "ad.jpg"},
            {"id": 4, "image1": ""}]});
        let second = json!({"last_data": 1, "datas": [
            {"id": 1, "title": "", "twitter_user_id": "tw", "twitter_id": 5,
             "created_at": "2020-01-01 00:00:00", "image1": "c.gif"}]});
        let (root, server) = serve(vec![html(profile), json(first), json(second)]);
        let http = http("Kabe-uchiroom", root);
        let files = media(extract(
            &http,
            Url::parse("https://kabe-uchiroom.com/mypage/?id=123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "b");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(
            files[1].url.as_str(),
            "https://kabe-uchiroom.com/accounts/upfile/3/123/b.PNG"
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "3_02_Hello.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("kabeuchi/tw 5")
        );
        assert_eq!(file.render(&files[2]).unwrap(), "1_01.gif");
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["3","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /mypage/?id=123 "));
        assert!(requests[1].starts_with("POST /get_posts.php "));
        assert!(requests[1].contains("target_id=77&type=uploads"));
        assert!(requests[1].ends_with("latest_post=&page_num=0"));
        assert!(requests[2].ends_with("latest_post=4&page_num=1"));
    }
}
