//! kabe-uchiroom users, mirroring gallery_dl/extractor/kabeuchi.py.
use crate::error;
use crate::sites::gdl::{captures, date, extr, group, nameext_into, parse_iso, re, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Value;
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
    name: "kabe-uchiroom",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    captures(re!(r"^https?://kabe-uchiroom\.com/mypage/?\?id=(\d+)"), url)
        .map(|c| group(&c, 1).into())
}

/// `items` for one post: up to six numbered images.
fn post_items(post: &Value, base: &str) -> Vec<Item> {
    if truthy(post.get("is_ad")) || !truthy(post.get("image1")) {
        return Vec::new();
    }
    let Some(mut fields) = post.as_object().cloned() else {
        return Vec::new();
    };
    fields.insert("date".into(), date(parse_iso(str_of(post, "created_at"))));
    let id = fields
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let mut items = Vec::new();
    for num in 1..=6 {
        let name = str_of(post, &format!("image{num}"));
        if name.is_empty() {
            break;
        }
        let mut fields = fields.clone();
        fields.insert("num".into(), num.into());
        nameext_into(name, &mut fields);
        if let Ok(url) = Url::parse(&format!("{base}{name}")) {
            items.push(Item::new(url, fields).ids(id.clone(), num.to_string()));
        }
    }
    items
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(uid) = target(&url) else {
        return crate::sites::fail(error("unsupported kabe-uchiroom URL"));
    };
    let base = format!("{ROOT}/accounts/upfile/{}/{uid}/", &uid[uid.len() - 1..]);
    crate::sites::paginate(Some((None::<String>, String::new(), 0u64)), move |state| {
        let uid = uid.clone();
        let base = base.clone();
        async move {
            let Some((target_id, latest, page)) = state else {
                return Ok(None);
            };
            let target_id = match target_id {
                Some(id) => id,
                None => {
                    let request = http.api(&format!("{ROOT}/mypage/?id={uid}"))?;
                    let (text, final_url) = http.text_final(&request, &[]).await?;
                    if final_url != request && final_url.as_str() == format!("{ROOT}/") {
                        return Err(error("kabe-uchiroom user not found"));
                    }
                    extr(&text, "user_friend_id = \"", "\"").to_owned()
                }
            };
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
            let info = http
                .post_form(&http.api(&format!("{ROOT}/get_posts.php"))?, &form, &[])
                .await?;
            let Some(datas) = info["datas"].as_array().filter(|d| !d.is_empty()) else {
                return Ok(None);
            };
            let last = crate::sites::scalar(&datas[datas.len() - 1]["id"]);
            let next = (last != crate::sites::scalar(&info["last_data"]))
                .then(|| (Some(target_id), last, page + 1));
            let events = datas
                .iter()
                .flat_map(|post| crate::sites::post(&SITE, http, &USER, post_items(post, &base)))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, media, serve};
    use serde_json::json;

    #[tokio::test]
    async fn pages_posts_until_last() {
        let post = |id: i64, title: &str| {
            json!({"id": id, "title": title, "created_at": "2020-01-02 03:04:05", "twitter_user_id": "1", "twitter_id": "jane",
            "image1": format!("{id}a.jpg"), "image2": format!("{id}b.png"), "image3": "", "image4": "x.jpg"})
        };
        let (root, server) = serve(vec![
            html(r#"var user_friend_id = "77";"#),
            json(
                json!({"datas": [post(5, "Hi"), {"id": 4, "is_ad": true, "image1": "ad.jpg"}], "last_data": 3}),
            ),
            json(json!({"datas": [post(3, "")], "last_data": 3})),
        ]);
        let http = client("kabeuchi", root);
        let files = media(extract(
            &http,
            Url::parse("https://kabe-uchiroom.com/mypage/?id=12345").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(
            files[0].url.as_str(),
            "https://kabe-uchiroom.com/accounts/upfile/5/12345/5a.jpg"
        );
        let (file, _) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "5_02_Hi.png");
        assert_eq!(file.render(&files[2]).unwrap(), "3_01.jpg");
        let requests = server.join().unwrap();
        assert!(requests[2].ends_with("latest_post=4&page_num=1"));
    }
}
