use gallery_dl_rs::{
    extract::Media,
    file_metadata::FileMetadata,
    twitter::{
        self, ArticleOptions, CardMode, CardOptions, ExpansionOptions, MediaOptions, VideoMode,
    },
};
use serde_json::{Value, json};
const ID: &str = "1234567890";
fn fixture() -> Value {
    serde_json::from_slice(include_bytes!("../fixtures/twitter/mixed.json")).unwrap()
}
fn items(value: &mut Value) -> &mut Vec<Value> {
    value["data"]["tweetResult"]["result"]["legacy"]["extended_entities"]["media"]
        .as_array_mut()
        .unwrap()
}
fn parse(value: &Value, media: MediaOptions) -> Vec<Media> {
    twitter::parse_response_with_media_options(
        &serde_json::to_vec(value).unwrap(),
        ID,
        ExpansionOptions::default(),
        media,
    )
    .unwrap()
}
fn file(media: &Media) -> &FileMetadata {
    media.metadata.as_ref().unwrap().file.as_ref().unwrap()
}

#[test]
fn photos_videos_gifs_and_posters_keep_owned_dimensions_actual_variant_and_context_metadata() {
    let mut data = fixture();
    items(&mut data)[0]["original_info"] = json!({"width":2048,"height":1365});
    items(&mut data)[0]["ext_alt_text"] = json!("A public alternative description");
    items(&mut data)[0]["sensitive_media_warning"] = json!({"adult_content":true,"other":false,"graphic_violence":true,"unknown":"PRIVATE_UNKNOWN"});
    items(&mut data)[0]["source_status_id_str"] = json!("42");
    items(&mut data)[0]["additional_media_info"] = json!({"source_user":{"user_results":{"result":{"core":{"screen_name":"source_name"},"private":"PRIVATE_UNKNOWN"}}}});
    items(&mut data)[1]["original_info"] = json!({"width":640,"height":480});
    items(&mut data)[1]["video_info"]["duration_millis"] = json!(1250);
    items(&mut data)[1]["video_info"]["variants"][0]["bitrate"] = json!(999999999u64);
    let mut gif = items(&mut data)[1].clone();
    gif["type"] = json!("animated_gif");
    gif["video_info"] = json!({"duration_millis":2500,"variants":[{"content_type":"video/mp4","url":"https://cdn.example/animation.mp4"}]});
    items(&mut data).push(gif);
    let media = parse(
        &data,
        MediaOptions {
            previews: true,
            ..Default::default()
        },
    );
    assert_eq!(media.len(), 6);
    let photo = file(&media[0]);
    assert_eq!(
        (
            photo.width,
            photo.height,
            photo.width_original,
            photo.height_original
        ),
        (Some(2048), Some(1365), Some(2048), Some(1365))
    );
    assert_eq!(photo.media_type.as_deref(), Some("photo"));
    assert_eq!(
        photo.description.as_deref(),
        Some("A public alternative description")
    );
    assert_eq!(
        photo
            .sensitive_flags
            .iter()
            .map(|flag| flag.as_ref())
            .collect::<Vec<_>>(),
        ["Nudity", "Sensitive", "Violence"]
    );
    assert_eq!(photo.source_id.as_deref(), Some("42"));
    assert_eq!(photo.source_username.as_deref(), Some("source_name"));
    let video = file(&media[1]);
    assert_eq!(video.media_type.as_deref(), Some("video"));
    assert_eq!(video.duration, Some(1.25));
    assert_eq!(video.bitrate, Some(2176000));
    assert_eq!((video.width, video.height), (Some(640), Some(480)));
    let poster = file(&media[2]);
    assert_eq!(poster.media_type.as_deref(), Some("preview"));
    assert_eq!((poster.duration, poster.bitrate), (None, None));
    assert_eq!(file(&media[4]).media_type.as_deref(), Some("animated_gif"));
    assert_eq!(file(&media[4]).duration, Some(2.5));
    assert_eq!(file(&media[4]).bitrate, None);
    assert_eq!(file(&media[5]).media_type.as_deref(), Some("preview"));
    assert!(
        !serde_json::to_string(&media)
            .unwrap()
            .contains("PRIVATE_UNKNOWN")
    );
}

#[test]
fn summary_and_article_files_use_only_explicit_dimensions_and_document_type() {
    let mut data = fixture();
    items(&mut data).truncate(1);
    data["data"]["tweetResult"]["result"]["card"] = json!({"name":"summary","binding_values":{"photo_image_full_size_original":{"image_value":{"url":"https://cdn.example/card.jpg","width":320,"height":"invalid","width_original":640,"height_original":480}}}});
    data["data"]["tweetResult"]["result"]["article"] = json!({"article_results":{"result":{
        "rest_id":"42","title":"Article","cover_media":{"media_info":{"original_img_url":"https://cdn.example/cover.jpg","original_img_width":111,"original_img_height":222}},
        "media_entities":[{"media_id":"7","media_info":{"original_img_url":"https://cdn.example/image.jpg","original_img_width":333,"original_img_height":444}}],
        "content_state":{"blocks":[{"key":"a","type":"unstyled","text":"Article text"}],"entityMap":[]}
    }}});
    let media = twitter::parse_response_with_article_options(
        &serde_json::to_vec(&data).unwrap(),
        ID,
        ExpansionOptions::default(),
        MediaOptions::default(),
        CardOptions {
            mode: CardMode::Files,
            blacklist: Vec::new(),
        },
        ArticleOptions {
            document: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(media.len(), 5);
    assert_eq!(
        (file(&media[1]).width, file(&media[1]).height),
        (Some(320), None)
    );
    assert_eq!(
        (
            file(&media[1]).width_original,
            file(&media[1]).height_original
        ),
        (Some(640), Some(480))
    );
    assert_eq!(file(&media[2]).media_type.as_deref(), Some("document"));
    assert_eq!(
        (file(&media[2]).width, file(&media[2]).height),
        (None, None)
    );
    assert_eq!(file(&media[3]).media_type.as_deref(), Some("article:cover"));
    assert_eq!(
        (file(&media[3]).width, file(&media[3]).height),
        (Some(111), Some(222))
    );
    assert_eq!(file(&media[4]).media_type.as_deref(), Some("article:image"));
    assert_eq!(
        (
            file(&media[4]).width_original,
            file(&media[4]).height_original
        ),
        (Some(333), Some(444))
    );
}

#[test]
fn unified_media_share_entity_metadata_but_raw_component_urls_do_not_fabricate_dimensions() {
    let mut data = fixture();
    items(&mut data).truncate(1);
    let encoded=json!({"media_entities":{"a":{"type":"photo","media_url_https":"https://cdn.example/card.jpg","original_info":{"width":600,"height":400},"ext_alt_text":"Card alt"}},"components":["a"],"component_objects":{"a":{"data":{"conversation_preview":[{"mediaUrls":["https://cdn.example/raw.jpg"]}]}}}}).to_string();
    data["data"]["tweetResult"]["result"]["card"] =
        json!({"name":"unified_card","binding_values":{"unified_card":{"string_value":encoded}}});
    let media = twitter::parse_response_with_card_options(
        &serde_json::to_vec(&data).unwrap(),
        ID,
        ExpansionOptions::default(),
        MediaOptions::default(),
        CardOptions {
            mode: CardMode::Files,
            blacklist: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(media.len(), 3);
    assert_eq!(
        (file(&media[1]).width, file(&media[1]).height),
        (Some(600), Some(400))
    );
    assert_eq!(file(&media[1]).description.as_deref(), Some("Card alt"));
    assert!(media[2].metadata.as_ref().unwrap().file.is_none());
}

#[test]
fn malformed_optional_fields_are_omitted_disabled_videos_stay_unparsed_and_identities_do_not_change()
 {
    let mut data = fixture();
    items(&mut data)[0]["original_info"] = json!({"width":-1,"height":"PRIVATE_DIMENSION"});
    items(&mut data)[0]["ext_alt_text"] = json!({"private":"invalid description"});
    items(&mut data)[0]["sensitive_media_warning"] = json!("private malformed flags");
    items(&mut data)[0]["source_status_id_str"] = json!("042");
    items(&mut data)[0]["additional_media_info"] = json!(false);
    items(&mut data)[1]["video_info"] = json!("private malformed disabled variants");
    let media = parse(
        &data,
        MediaOptions {
            videos: VideoMode::Skip,
            ..Default::default()
        },
    );
    assert_eq!(media.len(), 2);
    let metadata = file(&media[0]);
    assert_eq!(
        (
            metadata.width,
            metadata.height,
            metadata.source_id.as_deref(),
            metadata.description.as_deref()
        ),
        (None, None, None, None)
    );
    assert!(metadata.sensitive_flags.is_empty());
    for item in media {
        let mut without = item.clone();
        without.metadata.as_mut().unwrap().file = None;
        assert_eq!(item.archive_key(), without.archive_key());
        assert_eq!(item.identity(), without.identity());
        assert_eq!(item.filename, without.filename);
        assert!(
            !serde_json::to_string(&item)
                .unwrap()
                .contains("PRIVATE_DIMENSION")
        );
    }
}
