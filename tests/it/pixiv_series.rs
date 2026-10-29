use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv::{self, Target},
};
use serde_json::{Value, json};

fn compat(series: Value, num_series: i64) -> Media {
    extract::parse_messages(&serde_json::to_vec(&json!([[3,"https://i.pximg.net/42_p0.jpg",{
        "id":42,"num":0,"filename":"42_p0","extension":"jpg",
        "title":"Title","caption":"<p>Artwork caption</p>","series":series,"num_series":num_series
    }]])).unwrap(), "pixiv").unwrap().remove(0)
}

fn detail() -> Value {
    json!({"id":7,"title":"First series","caption":"<p>Raw series caption</p>","total":4})
}

#[test]
fn series_route_and_metadata_match_recorded_upstream_first_page_detail() {
    let reference: Value =
        serde_json::from_str(include_str!("../fixtures/pixiv/series.json")).unwrap();
    let url = extract::http_url(reference["url"].as_str().unwrap()).unwrap();
    assert_eq!(
        pixiv::target(&url).unwrap(),
        Target::Series {
            owner: "999".into(),
            id: "7".into()
        }
    );
    assert_eq!(extract::social_site(&url), Some("pixiv"));
    for work in reference["works"].as_array().unwrap() {
        let media = compat(work["series"].clone(), work["num_series"].as_i64().unwrap());
        let post = media.metadata.as_ref().unwrap();
        let series = post.series.as_ref().unwrap();
        assert_eq!(series.id, "7");
        assert_eq!(series.title.as_deref(), Some("First series"));
        assert_eq!(series.caption.as_deref(), Some("<p>Raw series caption</p>"));
        assert_eq!(series.total, 4);
        assert_eq!(post.num_series, work["num_series"].as_i64());
        assert_eq!(post.caption.as_deref(), Some("Title\n\nArtwork caption"));
    }
}

#[test]
fn series_media_share_artwork_archive_identity_independently_of_chapter_order() {
    let media = compat(detail(), 4);
    let mut direct = media.clone();
    direct.metadata.as_mut().unwrap().series = None;
    direct.metadata.as_mut().unwrap().num_series = None;
    direct.filename = "42_p0.jpg".into();
    assert!(media.archive_key().is_some());
    assert_eq!(media.archive_key(), direct.archive_key());
    for position in [-1, 0, 1, 10] {
        assert_eq!(
            media.archive_key(),
            compat(json!({"id":99,"title":"Different","total":100}), position).archive_key()
        );
    }
    direct.metadata.as_mut().unwrap().media_id = "42_p1".into();
    assert_ne!(media.archive_key(), direct.archive_key());
}

#[test]
fn series_fields_are_available_in_post_file_filters_and_source_number_templates() {
    let media = compat(detail(), -1);
    assert_eq!(media.filename, "0-1_42_p0.jpg");
    let filter = FilterExpression::parse("series['id'] == 7 and series['title'] == 'First series' and series['caption'] == '<p>Raw series caption</p>' and series['total'] == 4 and num_series == -1").unwrap();
    assert!(filter.matches(&media).unwrap());
    assert!(
        filter
            .matches_post(&Post::from_media(std::slice::from_ref(&media), "pixiv"))
            .unwrap()
    );
    assert!(
        FilterExpression::parse(
            "metadata['num_series'] == num_series and metadata['series']['id'] == series['id']"
        )
        .unwrap()
        .matches(&media)
        .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse("{num_series:>03}_{id}_p0.{extension}")
            .unwrap()
            .render(&media)
            .unwrap(),
        "0-1_42_p0.jpg"
    );
    let public = serde_json::to_value(&media).unwrap();
    assert_eq!(
        public["metadata"]["series"]["caption"],
        "<p>Raw series caption</p>"
    );
    assert_eq!(public["metadata"]["num_series"], -1);
}

#[test]
fn malformed_series_metadata_is_omitted_without_losing_artwork_archive_identity() {
    for value in [
        Value::Null,
        json!({}),
        json!({"id":"PRIVATE","total":4}),
        json!({"id":7,"total":-1}),
        json!({"id":7,"total":4,"title":{"private":"PRIVATE"}}),
    ] {
        let media = compat(value, 1);
        assert!(media.metadata.as_ref().unwrap().series.is_none());
        assert_eq!(media.filename, "42_p0.jpg");
        assert!(media.archive_key().is_some());
        assert!(!serde_json::to_string(&media).unwrap().contains("PRIVATE"));
    }
}
