//! `--list-extractors` names the upstream classes of every native extractor.
mod common;

fn listing(flag: &str) -> Vec<String> {
    let output = common::binary_command().arg(flag).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn dedicated_extractors_list_their_supported_subcategories() {
    let lines = listing("--list-extractors");
    let line = |category: &str| {
        lines
            .iter()
            .find(|line| line.split(':').next() == Some(category))
            .unwrap_or_else(|| panic!("{category} missing: {lines:?}"))
            .clone()
    };
    for (category, expected) in [
        (
            "deviantart",
            "deviantart: avatar, background, collection, deviation, favorite, folder, following, gallery, gallery-search, journal, scraps, search, stash, status, tag, user, watch, watch-posts",
        ),
        (
            "pixiv",
            "pixiv: artworks, avatar, background, favorite, followed, me, pixivision, ranking, search, series, sketch, unlisted, user, work",
        ),
        ("pixiv-novel", "pixiv-novel: bookmark, novel, series, user"),
        (
            "facebook",
            "facebook: albums, avatar, info, photo, photos, set, user, video",
        ),
        ("gelbooru", "gelbooru: favorite, pool, post, redirect, tag"),
        (
            "reddit",
            "reddit: home, image, redirect, submission, subreddit, user",
        ),
    ] {
        assert_eq!(line(category), expected);
    }
    for (category, subcategory) in [
        ("twitter", "event"),
        ("twitter", "image"),
        ("instagram", "stories-tray"),
        ("bluesky", "bookmark"),
        ("pawoo", "status"),
        ("coomer", "discord-server"),
        ("postimg", "gallery"),
    ] {
        let line = line(category);
        let listed = line.split_once(": ").unwrap().1;
        assert!(listed.split(", ").any(|s| s == subcategory), "{line}");
    }
    // Dedicated extractors list their classes in sorted, unique order.
    for category in [
        "instagram",
        "twitter",
        "facebook",
        "imgur",
        "bluesky",
        "danbooru",
        "e621",
        "safebooru",
        "kemono",
        "pixeldrain",
        "catbox",
        "imgbox",
        "tumblr",
        "flickr",
        "wallhaven",
        "artstation",
        "pinterest",
        "newgrounds",
        "furaffinity",
        "weasyl",
        "vsco",
    ] {
        let line = line(category);
        let names: Vec<_> = line.split_once(": ").unwrap().1.split(", ").collect();
        assert!(names.windows(2).all(|w| w[0] < w[1]), "{line}");
    }
    // `--list-modules` keeps bare category names.
    let modules = listing("--list-modules");
    assert!(modules.iter().any(|line| line == "deviantart"));
    assert!(!modules.iter().any(|line| line.contains(':')));
}
