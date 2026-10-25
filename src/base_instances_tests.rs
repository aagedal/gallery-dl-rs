//! Configured instance definitions, matching and URL prefixes.
//!
//! The registry is process-wide; every test uses its own hosts and names.
use super::*;
use serde_json::json;

fn definition(base: Base, category: &str, value: serde_json::Value) -> Result<Option<Definition>> {
    Definition::parse(base, category, &value)
}

fn url(raw: &str) -> Url {
    Url::parse(raw).unwrap()
}

#[test]
fn definitions_follow_upstream_and_reject_unsafe_roots_and_names() {
    let parsed = definition(
        Base::Danbooru,
        "defbooru",
        json!({"root": "https://defbooru.test/", "pattern": r"(?:www\.)?defbooru\.test"}),
    )
    .unwrap()
    .unwrap();
    assert_eq!(parsed.category, "defbooru");
    assert_eq!(parsed.root.as_str(), "https://defbooru.test/");
    assert_eq!(parsed.pattern.as_deref(), Some(r"(?:www\.)?defbooru\.test"));
    // Upstream only treats objects with a `root` as instances.
    for value in [
        json!("x"),
        json!({"metadata": true}),
        json!(["root"]),
        json!(null),
    ] {
        assert_eq!(definition(Base::Danbooru, "tag", value).unwrap(), None);
    }
    // Roots are http(s) origins without credentials.
    for root in [
        json!("ftp://defbooru.test"),
        json!("https://user:PRIVATE@defbooru.test"),
        json!("https://defbooru.test/booru"),
        json!("https://defbooru.test/?q=1"),
        json!("https://defbooru.test/#x"),
        json!("defbooru.test"),
        json!(null),
        json!(42),
    ] {
        let error = definition(Base::Danbooru, "defbooru", json!({"root": root}))
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("PRIVATE"), "{error}");
    }
    // Names must be new, bounded categories.
    for name in [
        "danbooru",
        "e621",
        "e926",
        "mastodon.social",
        "pawoo",
        "Danbooru",
        "mastodon",
        "twitter",
        "direct",
        "oauth",
        "",
        "../x",
        "a/b",
        ".hidden",
        "x.tag",
        "x.artist-search",
        &"x".repeat(65),
    ] {
        assert!(
            definition(Base::Danbooru, name, json!({"root": "https://x.test"})).is_err(),
            "{name}"
        );
    }
    // Unknown options and invalid patterns are rejected; tokens are
    // Mastodon-only.
    for (base, value) in [
        (
            Base::Danbooru,
            json!({"root": "https://x.test", "username": "PRIVATE"}),
        ),
        (
            Base::E621,
            json!({"root": "https://x.test", "access-token": "PRIVATE"}),
        ),
        (
            Base::Danbooru,
            json!({"root": "https://x.test", "pattern": "(unclosed"}),
        ),
        (
            Base::Danbooru,
            json!({"root": "https://x.test", "pattern": "x".repeat(2000)}),
        ),
        (
            Base::Danbooru,
            json!({"root": "https://x.test", "pattern": 1}),
        ),
    ] {
        let error = definition(base, "defbooru", value)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("PRIVATE"), "{error}");
    }
    let mastodon = definition(
        Base::Mastodon,
        "defdon",
        json!({"root": "https://defdon.test", "access-token": "token",
               "client-id": "id", "client-secret": "secret"}),
    )
    .unwrap()
    .unwrap();
    assert_eq!(mastodon.access_token.as_deref(), Some("token"));
    assert_eq!(mastodon.client_id.as_deref(), Some("id"));
    assert_eq!(mastodon.client_secret.as_deref(), Some("secret"));
    assert!(!format!("{mastodon:?}").contains("token"));
}

#[test]
fn configured_instances_match_their_pattern_or_exact_root_host() {
    register(&[
        definition(
            Base::Danbooru,
            "matchbooru",
            json!({"root": "https://matchbooru.test", "pattern": r"(?:www\.)?matchbooru\.(?:test|example)"}),
        )
        .unwrap()
        .unwrap(),
        definition(Base::E621, "match621", json!({"root": "http://match621.test:8080"}))
            .unwrap()
            .unwrap(),
        // A pattern can never claim a built-in instance's host.
        definition(
            Base::Danbooru,
            "claimbooru",
            json!({"root": "https://claim.test",
                   "pattern": r"(?:danbooru\.donmai\.us|e621\.net|e926\.net|booru\.allthefallen\.moe|mastodon\.social)"}),
        )
        .unwrap()
        .unwrap(),
    ]);
    let pattern = by_category("matchbooru").unwrap();
    assert!(!pattern.prefixed);
    assert_eq!(pattern.netloc, "matchbooru.test");
    assert_eq!(pattern.origin, "https://matchbooru.test");
    assert_eq!(pattern.static_host, "static1.matchbooru.test");
    assert_eq!(pattern.class_key("tag"), Some("matchbooru.tag"));
    assert_eq!(pattern.class_key("pool"), None);
    for raw in [
        "https://matchbooru.test/posts/1",
        "http://www.matchbooru.example/posts/1",
    ] {
        assert_eq!(find(&url(raw)), Some(pattern), "{raw}");
    }
    let exact = by_category("match621").unwrap();
    assert_eq!(exact.netloc, "match621.test:8080");
    assert_eq!(
        find(&url("https://match621.test:8080/posts/1")),
        Some(exact)
    );
    assert_eq!(find(&url("http://match621.test/posts/1")), None);
    assert_eq!(find(&url("https://claim.test/posts/1")), None);
    assert_eq!(by_netloc(Base::E621, "match621.test:8080"), Some(exact));
    assert_eq!(by_netloc(Base::Danbooru, "match621.test:8080"), None);
    for raw in [
        "https://danbooru.donmai.us/posts/1",
        "https://e621.net/posts/1",
        "https://e926.net/posts/1",
        "https://booru.allthefallen.moe/posts/1",
        "https://mastodon.social/@a",
        "https://user:pass@matchbooru.test/posts/1",
        "ftp://matchbooru.test/posts/1",
    ] {
        assert_eq!(find(&url(raw)), None, "{raw}");
    }
}

#[test]
fn prefixed_inputs_register_their_host_as_an_instance_of_the_family() {
    let input = crate::extract::input_url("Danbooru:https://prefix-a.test/posts/3?x=1").unwrap();
    assert_eq!(input.scheme(), "danbooru");
    assert_eq!(
        prefixed(&input),
        Some((Base::Danbooru, url("https://prefix-a.test/posts/3?x=1")))
    );
    let target = unprefix(input).unwrap();
    assert_eq!(target.as_str(), "https://prefix-a.test/posts/3?x=1");
    let instance = by_category("prefix-a.test").unwrap();
    assert!(instance.prefixed);
    assert_eq!(instance.base, Base::Danbooru);
    assert_eq!(instance.root.as_str(), "https://prefix-a.test/");
    assert_eq!(find(&target), Some(instance));
    // The lowercase spelling, ports and http roots, as upstream's
    // `(https?://[^/?#]+)` capture.
    let target =
        unprefix(crate::extract::input_url("e621:http://prefix-b.test:8080/posts/1").unwrap())
            .unwrap();
    let instance = by_category("prefix-b.test:8080").unwrap();
    assert_eq!(
        (instance.base, instance.root.as_str()),
        (Base::E621, "http://prefix-b.test:8080/")
    );
    assert_eq!(find(&target), Some(instance));
    let target =
        unprefix(crate::extract::input_url("mastodon:https://prefix-c.test/@alice").unwrap())
            .unwrap();
    assert_eq!(find(&target).unwrap().base, Base::Mastodon);
    // Repeating an input reuses the instance.
    unprefix(crate::extract::input_url("Danbooru:https://prefix-a.test/pools/1").unwrap()).unwrap();
    assert_eq!(
        registry()
            .iter()
            .filter(|known| known.category == "prefix-a.test")
            .count(),
        1
    );
    // Built-in hosts of the same family keep their instance; other
    // families' hosts are refused.
    assert_eq!(
        unprefix(crate::extract::input_url("Danbooru:https://danbooru.donmai.us/posts/1").unwrap())
            .unwrap()
            .as_str(),
        "https://danbooru.donmai.us/posts/1"
    );
    assert_eq!(by_category("danbooru.donmai.us"), None);
    for raw in [
        "E621:https://danbooru.donmai.us/posts/1",
        "mastodon:https://e621.net/posts/1",
        "Danbooru:https://mastodon.social/@a",
        "e621:https://prefix-a.test/posts/1",
    ] {
        assert!(
            unprefix(crate::extract::input_url(raw).unwrap()).is_err(),
            "{raw}"
        );
    }
    // Upstream requires an explicit http(s) target; credentials are refused.
    for raw in [
        "Danbooru:prefix-d.test/posts/1",
        "Danbooru:ftp://prefix-d.test/",
        "Danbooru:https://user:pass@prefix-d.test/posts/1",
        "DANBOORU:https://prefix-d.test/posts/1",
    ] {
        assert!(
            crate::extract::input_url(raw)
                .ok()
                .and_then(|url| prefixed(&url))
                .is_none(),
            "{raw}"
        );
    }
    // Plain URLs pass through unchanged.
    let plain = url("https://prefix-e.test/posts/1");
    assert_eq!(unprefix(plain.clone()).unwrap(), plain);
    assert_eq!(by_category("prefix-e.test"), None);
}
