use gallery_dl_rs::{
    extract::{Media, MediaJob},
    twitter::{
        ArticleOptions, CardMode, CardOptions, ExpansionOptions, ImageOptions, MediaOptions,
        VideoMode, parse_response_with_image_options,
    },
};
use serde_json::{Value, json};
const ID: &str = "1234567890";
fn fixture(name: &str) -> Value {
    let bytes = match name {
        "mixed" => include_bytes!("../fixtures/twitter/mixed.json").as_slice(),
        _ => include_bytes!("../fixtures/twitter/photo.json").as_slice(),
    };
    serde_json::from_slice(bytes).unwrap()
}
fn parse(
    data: &Value,
    images: ImageOptions,
    media: MediaOptions,
    cards: CardMode,
    article: ArticleOptions,
) -> Vec<Media> {
    parse_response_with_image_options(
        &serde_json::to_vec(data).unwrap(),
        ID,
        ExpansionOptions::default(),
        media,
        CardOptions {
            mode: cards,
            blacklist: Vec::new(),
        },
        article,
        images,
    )
    .unwrap()
}
fn custom(sizes: &[&str]) -> ImageOptions {
    ImageOptions::new(sizes.iter().map(|size| (*size).into()).collect()).unwrap()
}
fn fallbacks(media: &Media) -> Vec<url::Url> {
    media
        .handoff
        .get("_fallback")
        .map(|value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|value| url::Url::parse(value.as_str().unwrap()).unwrap())
                .collect()
        })
        .unwrap_or_default()
}
fn size(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "name")
        .map(|(_, value)| value.into_owned())
}

#[test]
fn default_size_chain_is_private_and_independent_of_social_file_archive_identity() {
    let data = fixture("photo");
    let first = parse(
        &data,
        ImageOptions::default(),
        MediaOptions::default(),
        CardMode::Skip,
        ArticleOptions::default(),
    );
    assert_eq!(size(&first[0].url).as_deref(), Some("orig"));
    assert_eq!(
        fallbacks(&first[0]).iter().map(size).collect::<Vec<_>>(),
        [
            Some("4096x4096".into()),
            Some("large".into()),
            Some("medium".into()),
            Some("small".into())
        ]
    );
    let selected = parse(
        &data,
        custom(&["small", "orig"]),
        MediaOptions::default(),
        CardMode::Skip,
        ArticleOptions::default(),
    );
    assert_eq!(first[0].archive_key(), selected[0].archive_key());
    assert_eq!(first[0].filename, selected[0].filename);
    assert_eq!(
        first[0].metadata.as_ref().unwrap().index,
        selected[0].metadata.as_ref().unwrap().index
    );
    assert_ne!(first[0].identity(), selected[0].identity());
    let public = serde_json::to_string(&first).unwrap();
    assert!(!public.contains("_fallback"));
    assert!(!public.contains("4096x4096"));
    assert!(!public.contains("User-Agent"));
}

#[test]
fn custom_sizes_preserve_order_duplicates_signed_queries_and_media_numbering() {
    let mut data = fixture("mixed");
    data["data"]["tweetResult"]["result"]["legacy"]["extended_entities"]["media"][0]["media_url_https"] =
        json!("https://cdn.example/photo?format=webp&token=a%2Fb&name=old&after=keep&name=other");
    let media = parse(
        &data,
        custom(&["large", "small", "large"]),
        MediaOptions {
            previews: true,
            ..Default::default()
        },
        CardMode::Skip,
        ArticleOptions::default(),
    );
    assert_eq!(media.len(), 4);
    assert_eq!(
        media
            .iter()
            .map(|item| item.metadata.as_ref().unwrap().index)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(media[0].filename, format!("{ID}_1.webp"));
    for url in std::iter::once(&media[0].url).chain(fallbacks(&media[0]).iter()) {
        let pairs: Vec<_> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            &pairs[..3],
            &[
                ("format".into(), "webp".into()),
                ("token".into(), "a/b".into()),
                ("after".into(), "keep".into())
            ]
        );
        assert_eq!(pairs.iter().filter(|(key, _)| key == "name").count(), 1);
    }
    assert_eq!(
        fallbacks(&media[0]).iter().map(size).collect::<Vec<_>>(),
        [Some("small".into()), Some("large".into())]
    );
    assert!(media[1].url.path().ends_with(".mp4"));
    assert!(media[1].handoff.is_empty());
    assert_eq!(size(&media[2].url).as_deref(), Some("large"));
    assert_eq!(fallbacks(&media[2]).len(), 2);
}

#[test]
fn one_size_has_no_fallback_and_delegate_video_targets_are_never_resized() {
    let data = fixture("mixed");
    let media = parse(
        &data,
        custom(&["thumb_size-1"]),
        MediaOptions {
            videos: VideoMode::Ytdl,
            previews: true,
            ..Default::default()
        },
        CardMode::Skip,
        ArticleOptions::default(),
    );
    assert_eq!(media.len(), 4);
    assert!(media.iter().all(|item| item.handoff.is_empty()));
    assert_eq!(
        media[1].url.as_str(),
        format!("ytdl:https://x.com/i/web/status/{ID}")
    );
    assert!(matches!(&media[1].job,MediaJob::VideoPage{page_url} if page_url.query().is_none()));
    assert_eq!(size(&media[0].url).as_deref(), Some("thumb_size-1"));
    assert_eq!(size(&media[2].url).as_deref(), Some("thumb_size-1"));
}

#[test]
fn summary_sizes_require_existing_name_and_component_urls_remain_raw() {
    for named in [false, true] {
        let mut data = fixture("photo");
        let url = if named {
            "https://cdn.example/card.jpg?token=s&name=small"
        } else {
            "https://cdn.example/card.jpg?token=s"
        };
        data["data"]["tweetResult"]["result"]["card"] = json!({"name":"summary","binding_values":{"photo_image_full_size_original":{"image_value":{"url":url}}}});
        let media = parse(
            &data,
            custom(&["large", "small"]),
            MediaOptions::default(),
            CardMode::Files,
            ArticleOptions::default(),
        );
        assert_eq!(
            size(&media[1].url),
            if named { Some("large".into()) } else { None }
        );
        assert_eq!(fallbacks(&media[1]).len(), usize::from(named));
    }
    let mut data = fixture("photo");
    let encoded=json!({"components":["c"],"component_objects":{"c":{"data":{"conversation_preview":[{"mediaUrls":["https://cdn.example/raw.jpg?name=small&token=s"]}]}}}}).to_string();
    data["data"]["tweetResult"]["result"]["card"] =
        json!({"name":"unified_card","binding_values":{"unified_card":{"string_value":encoded}}});
    let media = parse(
        &data,
        custom(&["large", "orig"]),
        MediaOptions::default(),
        CardMode::Files,
        ArticleOptions::default(),
    );
    assert_eq!(
        media[1].url.as_str(),
        "https://cdn.example/raw.jpg?name=small&token=s"
    );
    assert!(media[1].handoff.is_empty());
}

#[test]
fn article_files_and_html_use_first_size_without_embedding_fallbacks_in_documents() {
    let mut data = fixture("photo");
    data["data"]["tweetResult"]["result"]["article"] = json!({"article_results":{"result":{
        "rest_id":"101","title":"Article","cover_media":{"media_info":{"original_img_url":"https://cdn.example/cover.jpg?token=s&name=small"}},
        "media_entities":[{"media_id":"9","media_info":{"original_img_url":"https://cdn.example/article.jpg?name=small"}}],
        "content_state":{"entityMap":[{"key":"0","value":{"type":"MEDIA","data":{"mediaItems":[{"mediaId":"9"}]}}}],"blocks":[{"key":"a","type":"atomic","entityRanges":[{"key":0}]}]}
    }}});
    let media = parse(
        &data,
        custom(&["large", "fallback_only"]),
        MediaOptions::default(),
        CardMode::Skip,
        ArticleOptions {
            html: true,
            document: true,
            ..Default::default()
        },
    );
    assert_eq!(media.len(), 4);
    assert!(media[1].handoff.is_empty());
    let document = match &media[1].job {
        MediaJob::InlineText { data } => data,
        _ => panic!("expected document"),
    };
    assert!(document.contains("name=large"));
    assert!(!document.contains("fallback_only"));
    let html = media[0]
        .metadata
        .as_ref()
        .unwrap()
        .article
        .as_ref()
        .unwrap()
        .html
        .as_ref()
        .unwrap();
    assert!(html.contains("name=large"));
    assert!(!html.contains("fallback_only"));
    for file in &media[2..] {
        assert_eq!(size(&file.url).as_deref(), Some("large"));
        assert_eq!(fallbacks(file).len(), 1);
    }
    assert_eq!(media[2].url.path(), "/cover.jpg");
    assert_eq!(media[3].url.path(), "/article.jpg");
}

#[test]
fn size_constructor_rejects_unbounded_or_unsafe_values_without_echoing_input() {
    for values in [
        vec![],
        vec!["private/secret".into()],
        vec!["".into()],
        vec!["é".into()],
        vec!["a".repeat(33)],
        vec!["orig".into(); 33],
    ] {
        let error = ImageOptions::new(values).unwrap_err().to_string();
        assert!(!error.contains("private/secret"));
        assert!(error.contains("image sizes"));
    }
    let options = custom(&["orig", "orig", "a_B-2"]);
    assert_eq!(options.sizes(), ["orig", "orig", "a_B-2"]);
}
