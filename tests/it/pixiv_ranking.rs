use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv::{self, RankingMetadata, Target},
};
use serde_json::{Value, json};
use std::sync::Arc;

fn compat(ranking: Option<Value>) -> Media {
    let mut data = json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg","title":"Title","caption":"<p>Description</p>"});
    if let Some(ranking) = ranking {
        data["ranking"] = ranking;
    }
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://i.pximg.net/42_p0.jpg", data]])).unwrap(),
        "pixiv",
    )
    .unwrap()
    .remove(0)
}

fn ranking() -> Value {
    json!({"mode":"day","date":"2026-10-20","rank":3,"type":"all"})
}

#[test]
fn ranking_options_agree_with_recorded_upstream_metadata() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/pixiv/ranking.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let url = extract::http_url(case["url"].as_str().unwrap()).unwrap();
        let target = pixiv::target(&url);
        if case["error"] == true {
            assert!(target.is_err(), "{case}");
            continue;
        }
        let Target::Ranking(options) = target.unwrap() else {
            panic!("{case}")
        };
        let expected = &case["metadata"];
        assert_eq!(options.mode, expected["mode"]);
        if case["fallback_date"] != true {
            assert_eq!(options.date, expected["date"]);
        }
        assert_eq!(
            options
                .content
                .as_deref()
                .filter(|v| !v.is_empty())
                .unwrap_or("all"),
            expected["type"]
        );
        assert_eq!(extract::social_site(&url), Some("pixiv"));
    }
}

#[test]
fn ranking_archives_match_native_context_and_ignore_rank_or_content_filter() {
    let compat = compat(Some(ranking()));
    let mut native = compat.clone();
    native.metadata.as_mut().unwrap().ranking_archive = false;
    native.metadata.as_mut().unwrap().ranking = Some(Arc::new(RankingMetadata {
        mode: "day".into(),
        date: "2026-10-20".into(),
        rank: 999,
        kind: "manga".into(),
    }));
    assert!(native.archive_key().is_some());
    assert_eq!(native.archive_key(), compat.archive_key());
    let key = compat.archive_key();
    assert_ne!(key, self::compat(None).archive_key());
    for (field, value) in [("mode", json!("week")), ("date", json!("2026-10-19"))] {
        let mut other = ranking();
        other[field] = value;
        assert_ne!(key, self::compat(Some(other)).archive_key());
    }
    native.metadata.as_mut().unwrap().media_id = "42_p1".into();
    assert_ne!(key, native.archive_key());
    let mut public = serde_json::to_value(&compat).unwrap();
    assert_eq!(public["metadata"]["ranking"], ranking());
    assert!(
        public["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("ranking_archive")
            .is_none()
    );
}

#[test]
fn malformed_ranking_context_never_falls_back_to_an_ordinary_artwork_archive() {
    for invalid in [
        Value::Null,
        json!({}),
        json!("PRIVATE"),
        json!({"mode":"PRIVATE","date":"2026-10-20","rank":1,"type":"all"}),
        json!({"mode":"day","date":"PRIVATE","rank":1,"type":"all"}),
        json!({"mode":"day","date":"2026-10-20","rank":-1,"type":"all"}),
        json!({"mode":"day","date":"2026-10-20","rank":1,"type":{"token":"PRIVATE"}}),
    ] {
        let media = compat(Some(invalid));
        assert!(media.archive_key().is_none());
        assert!(media.metadata.as_ref().unwrap().ranking.is_none());
        assert!(!serde_json::to_string(&media).unwrap().contains("PRIVATE"));
    }
    assert!(compat(None).archive_key().is_some());
}

#[test]
fn ranking_context_is_available_to_file_and_post_filters_and_templates() {
    let media = compat(Some(ranking()));
    assert_eq!(
        media.metadata.as_ref().unwrap().caption.as_deref(),
        Some("Title\n\nDescription")
    );
    let filter = FilterExpression::parse("ranking['mode'] == 'day' and ranking['date'] == '2026-10-20' and ranking['rank'] == 3 and ranking['type'] == 'all'").unwrap();
    assert!(filter.matches(&media).unwrap());
    assert!(
        filter
            .matches_post(&Post::from_media(std::slice::from_ref(&media), "pixiv"))
            .unwrap()
    );
    assert!(
        FilterExpression::parse("metadata['ranking']['rank'] == ranking['rank']")
            .unwrap()
            .matches(&media)
            .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse(
            "{ranking[mode]}_{ranking[date]}_{ranking[rank]:03}_{id}.{extension}"
        )
        .unwrap()
        .render(&media)
        .unwrap(),
        "day_2026-10-20_003_42.jpg"
    );
}
