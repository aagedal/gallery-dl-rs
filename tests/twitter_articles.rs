use gallery_dl_rs::{
    extract::{Media, MediaJob},
    twitter::{
        ArticleOptions, CardMode, CardOptions, ExpansionOptions, MediaOptions, RetweetMode,
        parse_response, parse_response_with_article_options,
    },
};
use serde_json::{Value, json};

const ID: &str = "1234567890";
fn image(id: &str, url: &str) -> Value {
    json!({"media_id":id,"media_key":format!("3_{id}"),"media_info":{"original_img_url":url,"original_img_width":600,"original_img_height":400}})
}
fn article(title: &str) -> Value {
    json!({"article_results":{"result":{
        "rest_id":"500","title":title,
        "metadata":{"first_published_at_secs":1700000000},
        "lifecycle_state":{"modified_at_secs":"1700000100"},
        "cover_media":image("600","https://cdn.example/cover.jpg?token=signature&name=small"),
        "media_entities":[image("601","https://cdn.example/a.png"),image("602","https://cdn.example/b.webp")],
        "content_state":{"blocks":[{"key":"block","type":"unstyled","text":"Text <script> & snow 雪","entityRanges":[],"inlineStyleRanges":[]}],"entityMap":[]}
    }}})
}
fn source(ordinary: bool) -> Value {
    let mut data: Value =
        serde_json::from_slice(include_bytes!("fixtures/twitter/photo.json")).unwrap();
    let root = &mut data["data"]["tweetResult"]["result"];
    root["article"] = article("Article <Title>");
    if !ordinary {
        root["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
    }
    data
}
fn parse(data: &Value, options: ArticleOptions) -> gallery_dl_rs::Result<Vec<Media>> {
    parse_response_with_article_options(
        &serde_json::to_vec(data).unwrap(),
        ID,
        ExpansionOptions::default(),
        MediaOptions::default(),
        CardOptions::default(),
        options,
    )
}

#[test]
fn articles_default_to_metadata_cover_then_embedded_media_and_preserve_numbering() {
    let data = source(true);
    let items = parse_response(&serde_json::to_vec(&data).unwrap(), ID).unwrap();
    assert_eq!(
        items.iter().map(|m| m.url.path()).collect::<Vec<_>>(),
        ["/media/fixture.jpg", "/cover.jpg", "/a.png", "/b.webp"]
    );
    assert_eq!(items[1].url.query(), Some("token=signature&name=orig"));
    for (i, item) in items.iter().enumerate() {
        let post = item.metadata.as_ref().unwrap();
        assert_eq!(post.post_id, ID);
        assert_eq!(post.index, i + 1);
        assert_eq!(post.media_id, (i + 1).to_string());
        let article = post.article.as_ref().unwrap();
        assert_eq!(article.id.as_deref(), Some("500"));
        assert_eq!(article.title.as_deref(), Some("Article <Title>"));
        assert_eq!(article.date, Some(1700000000));
        assert_eq!(article.date_updated, Some(1700000100));
        assert!(article.html.is_none());
        assert!(item.archive_key().is_some());
    }
    assert_eq!(
        parse(&source(false), ArticleOptions::default())
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn article_documents_follow_cards_then_precede_cover_and_media_with_shared_html_metadata() {
    let mut data = source(true);
    data["data"]["tweetResult"]["result"]["card"] = json!({"name":"summary","binding_values":{"photo_image_full_size_original":{"image_value":{"url":"https://cdn.example/card.jpg"}}}});
    let options = ArticleOptions {
        document: true,
        html: true,
        ..Default::default()
    };
    let items = parse_response_with_article_options(
        &serde_json::to_vec(&data).unwrap(),
        ID,
        ExpansionOptions::default(),
        MediaOptions::default(),
        CardOptions {
            mode: CardMode::Files,
            blacklist: vec![],
        },
        options,
    )
    .unwrap();
    assert_eq!(items.len(), 6);
    assert_eq!(items[0].url.path(), "/media/fixture.jpg");
    assert_eq!(items[1].url.path(), "/card.jpg");
    match &items[2].job {
        MediaJob::InlineText { data } => {
            assert!(data.contains("<!DOCTYPE html>"));
            assert!(data.contains("Article &lt;Title&gt;"));
            assert!(!data.contains("Text <script>"));
        }
        job => panic!("unexpected job: {job:?}"),
    }
    assert_eq!(items[2].filename, format!("{ID}_3.html"));
    assert_eq!(items[3].url.path(), "/cover.jpg");
    assert_eq!(items[4].url.path(), "/a.png");
    assert_eq!(items[5].url.path(), "/b.webp");
    let html = items[0]
        .metadata
        .as_ref()
        .unwrap()
        .article
        .as_ref()
        .unwrap()
        .html
        .as_ref()
        .unwrap();
    assert!(html.contains("Text &lt;script&gt; &amp; snow 雪"));
    for item in &items[1..] {
        assert!(std::sync::Arc::ptr_eq(
            html,
            item.metadata
                .as_ref()
                .unwrap()
                .article
                .as_ref()
                .unwrap()
                .html
                .as_ref()
                .unwrap()
        ));
        let first = items[0].metadata.as_ref().unwrap();
        let next = item.metadata.as_ref().unwrap();
        assert!(std::sync::Arc::ptr_eq(
            first.caption.as_ref().unwrap(),
            next.caption.as_ref().unwrap()
        ));
        assert!(std::sync::Arc::ptr_eq(
            first.username.as_ref().unwrap(),
            next.username.as_ref().unwrap()
        ));
    }
}

#[test]
fn article_options_parse_only_enabled_fields_and_disabled_articles_ignore_malformed_envelopes() {
    let mut data = source(true);
    let article =
        &mut data["data"]["tweetResult"]["result"]["article"]["article_results"]["result"];
    article["cover_media"] = json!("private malformed cover");
    article["media_entities"] = json!("private malformed media");
    article["content_state"] = json!("private malformed content");
    let items = parse(
        &data,
        ArticleOptions {
            metadata: true,
            ..ArticleOptions::disabled()
        },
    )
    .unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].metadata.as_ref().unwrap().article.is_some());
    let recovered = parse(&data, ArticleOptions::default()).unwrap();
    assert_eq!(recovered.len(), 1);
    assert!(
        !serde_json::to_string(&recovered)
            .unwrap()
            .contains("private")
    );
    data["data"]["tweetResult"]["result"]["article"] = json!("private malformed envelope");
    let items = parse(&data, ArticleOptions::disabled()).unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].metadata.as_ref().unwrap().article.is_none());
}

#[test]
fn article_media_use_outer_repost_or_original_source_and_keep_archive_context() {
    let data = source(true);
    let mut original = data["data"]["tweetResult"]["result"].clone();
    original["rest_id"] = json!("200");
    original["legacy"]["id_str"] = json!("200");
    original["article"] = article("Original");
    let mut outer = data;
    outer["data"]["tweetResult"]["result"]["article"] = article("Repost");
    outer["data"]["tweetResult"]["result"]["legacy"]["retweeted_status_id_str"] = json!("200");
    outer["data"]["tweetResult"]["result"]["legacy"]["retweeted_status_result"] =
        json!({"result":original});
    for (retweets, title, post_id) in [
        (RetweetMode::Repost, "Repost", ID),
        (RetweetMode::Original, "Original", "200"),
    ] {
        let items = parse_response_with_article_options(
            &serde_json::to_vec(&outer).unwrap(),
            ID,
            ExpansionOptions {
                retweets,
                quoted: false,
            },
            MediaOptions::default(),
            CardOptions::default(),
            ArticleOptions::default(),
        )
        .unwrap();
        assert_eq!(items.len(), 4);
        for item in items {
            let post = item.metadata.as_ref().unwrap();
            assert_eq!(post.post_id, post_id);
            assert_eq!(post.article.as_ref().unwrap().title.as_deref(), Some(title));
            assert_eq!(
                post.retweet_id.as_deref(),
                if retweets == RetweetMode::Repost {
                    Some("200")
                } else {
                    None
                }
            );
            assert!(item.archive_key().is_some());
        }
    }
}
