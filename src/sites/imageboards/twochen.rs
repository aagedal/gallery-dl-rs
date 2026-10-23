//! 2chen boards (sturdychan, schan), mirroring gallery_dl/extractor/2chen.py
//! by parsing HTML pages.
use super::{
    Extr, NO_DATE, component, datetime_string, digits, extract_iter, host_in, rest, set_nameext,
};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static THREAD: Route = Route::new(
    "thread",
    "{time} {filename}.{extension}",
    &["{category}", "{board}", "{thread} {title}"],
    "{board}_{thread}_{no}_{time}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&THREAD, &BOARD],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(STURDYCHAN, "sturdychan", "sturdychan");
instance!(SCHAN, "schan", "schan");

fn site(category: &str) -> &'static Site {
    match category {
        "sturdychan" => &STURDYCHAN,
        _ => &SCHAN,
    }
}

fn hosts(category: &str) -> &'static [&'static str] {
    match category {
        "sturdychan" => &["sturdychan.help", "2chen.moe", "2chen.club"],
        _ => &["schan.help"],
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "sturdychan" => "https://sturdychan.help",
        _ => "https://schan.help",
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    Thread { board: String, thread: String },
    Board { board: String },
}

fn target(category: &str, url: &Url) -> Option<Target> {
    if !host_in(url, hosts(category)) {
        return None;
    }
    let (board, tail) = component(rest(url))?;
    if let Some((thread, _)) = tail.strip_prefix('/').and_then(digits) {
        return Some(Target::Thread {
            board,
            thread: thread.into(),
        });
    }
    (matches!(tail, "" | "/") || tail.starts_with("/catalog")).then_some(Target::Board { board })
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let root = root(category);
    match target(category, &url) {
        Some(Target::Thread { board, thread }) => crate::sites::once(async move {
            let api = http.api(&format!("{root}/{board}/{thread}"))?;
            let page = http.text(&api).await?;
            let items = parse_thread(root, &page, &thread)?;
            Ok(crate::sites::events(crate::sites::post(
                site(category),
                http,
                &THREAD,
                items,
            )))
        }),
        Some(Target::Board { board }) => crate::sites::once(async move {
            let api = http.api(&format!("{root}/{board}/catalog"))?;
            let page = http.text(&api).await?;
            let children = extract_iter(&page, "<figure><a href=\"", "\"")
                .into_iter()
                .filter_map(|thread| {
                    Some((Url::parse(&format!("{root}{thread}")).ok()?, Map::new()))
                })
                .collect();
            Ok(super::queue_all(http, site(category), &BOARD, children))
        }),
        None => crate::sites::fail(error("unsupported 2chen URL")),
    }
}

fn parse_thread(root: &str, page: &str, thread: &str) -> Result<Vec<Item>> {
    let (board, after) = crate::sites::extr(page, "class=\"board\">/", "/<")
        .map(|board| (board, page.find("class=\"board\">/").unwrap_or_default()))
        .ok_or_else(|| error("2chen thread page lacks its board"))?;
    let title = crate::sites::extr(&page[after..], "<h3>", "</h3>")
        .ok_or_else(|| error("2chen thread page lacks its title"))?;
    let title = crate::sites::unescape(title);
    let mut items = Vec::new();
    for html in extract_iter(page, "class=\"glass media", "</article>") {
        let mut post = parse_post(html);
        let link = post.get("url").and_then(Value::as_str).unwrap_or_default();
        let link = match link.strip_prefix('/') {
            Some(path) => format!("{root}/{path}"),
            None => link.to_owned(),
        };
        let link = link.split('?').next().unwrap_or_default().to_owned();
        let Some(url) = Url::parse(&link).ok().filter(crate::sites::plain) else {
            continue;
        };
        let time = post.get("date").and_then(Value::as_i64).unwrap_or(NO_DATE);
        post.insert("date".into(), datetime_string(time).into());
        post.insert("time".into(), time.into());
        post.insert("url".into(), link.into());
        post.insert("board".into(), board.into());
        post.insert("thread".into(), thread.into());
        post.insert("title".into(), title.clone().into());
        let filename = post
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        set_nameext(&mut post, &filename);
        let no = post.get("no").map(crate::sites::scalar).unwrap_or_default();
        let name = post.get("name").and_then(Value::as_str).map(str::to_owned);
        items.push(
            Item::new(url, post)
                .ids(thread, no)
                .user(name)
                .timestamp(Some(time))
                .header("Referer", &format!("{root}/")),
        );
    }
    Ok(items)
}

fn parse_post(html: &str) -> Map<String, Value> {
    let mut extr = Extr::new(html);
    let mut post = Map::new();
    post.insert(
        "name".into(),
        crate::sites::unescape(extr.next("<span>", "</span>")).into(),
    );
    let date = extr.next("<time", "<");
    let date = date.split_once('>').map_or("", |(_, tail)| tail);
    post.insert("date".into(), parse_date(date).unwrap_or(NO_DATE).into());
    post.insert("no".into(), extr.next("href=\"#p", "\"").into());
    post.insert(
        "filename".into(),
        crate::sites::unescape(extr.next("download=\"", "\"")).into(),
    );
    let figure = extr.next("<figure>", "</");
    post.insert(
        "url".into(),
        crate::sites::extr(figure, "href=\"", "\"")
            .unwrap_or_default()
            .into(),
    );
    post.insert("hash".into(), extr.next("data-hash=\"", "\"").into());
    post
}

/// `strptime("%d %b %Y (%a) %H:%M:%S")` as a UTC Unix timestamp.
fn parse_date(text: &str) -> Option<i64> {
    let mut parts = text.split_whitespace();
    let day: u8 = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| name.eq_ignore_ascii_case(month))?;
    let year: i32 = parts.next()?.parse().ok()?;
    let weekday = parts.next()?;
    if !(weekday.starts_with('(') && weekday.ends_with(')')) {
        return None;
    }
    let mut clock = parts.next()?.split(':');
    let mut field = || clock.next()?.parse::<u8>().ok();
    let (hour, minute, second) = (field()?, field()?, field()?);
    let date = time::Date::from_calendar_date(
        year,
        time::Month::try_from(u8::try_from(month + 1).ok()?).ok()?,
        day,
    )
    .ok()?;
    let at = time::Time::from_hms(hour, minute, second).ok()?;
    parts
        .next()
        .is_none()
        .then(|| date.with_time(at).assume_utc().unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("sturdychan", "https://2chen.moe/a/12345/"),
            Some(Target::Thread {
                board: "a".into(),
                thread: "12345".into()
            })
        );
        assert_eq!(
            t("sturdychan", "https://sturdychan.help/a/catalog"),
            Some(Target::Board { board: "a".into() })
        );
        assert_eq!(
            t("schan", "https://schan.help/b"),
            Some(Target::Board { board: "b".into() })
        );
        for (c, raw) in [
            ("sturdychan", "https://sturdychan.help/"),
            ("sturdychan", "https://sturdychan.help/a/x"),
            ("sturdychan", "https://sturdychan.help/a/?x=1"),
            ("sturdychan", "https://schan.help/a/1"),
            ("schan", "https://2chen.moe/a/1"),
            ("schan", "https://schan.help.evil.test/a/1"),
            ("schan", "https://schan.help:8443/a/1"),
        ] {
            assert_eq!(t(c, raw), None, "{raw}");
        }
        let find = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(find("https://2chen.club/a/1"), Some("sturdychan"));
        assert_eq!(find("https://schan.help/a/"), Some("schan"));
    }

    #[test]
    fn parses_dates_like_strptime() {
        assert_eq!(
            parse_date("02 Jan 2023 (Mon) 03:04:05"),
            Some(1_672_628_645)
        );
        assert_eq!(
            parse_date("2 Jan 2023 (Mon) 03:04:05 "),
            Some(1_672_628_645)
        );
        assert_eq!(parse_date("32 Jan 2023 (Mon) 03:04:05"), None);
        assert_eq!(parse_date("02 Foo 2023 (Mon) 03:04:05"), None);
        assert_eq!(parse_date("02 Jan 2023 Mon 03:04:05"), None);
        assert_eq!(parse_date(""), None);
    }

    const PAGE: &str = r##"<div class="board">/a/</div><h3>Title &amp; more</h3>
<article class="glass media"><span>Anon &amp; co</span> <time datetime="x">02 Jan 2023 (Mon) 03:04:05</time>
<a href="#p100">No</a> <a download="my pic.PNG" href="/x"></a>
<figure><a href="/files/abc.png?v=2">img</a></figure> <div data-hash="h1"></div></article>
<article class="glass media"><span>Bob</span> <time>03 Jan 2023 (Tue) 00:00:00</time>
<a href="#p101">No</a> <figure></figure></article>
<article class="glass media"><span>Eve</span> <time>03 Jan 2023 (Tue) 00:00:01</time>
<a href="#p102">No</a> <a download="z.jpg"></a>
<figure><a href="https://cdn.example.test/z.jpg">img</a></figure></article>"##;

    #[tokio::test]
    async fn extracts_thread_files() {
        let (origin, server) = serve(vec![html(PAGE)]);
        let http = http("sturdychan", origin);
        let files = media(extract(
            "sturdychan",
            &http,
            Url::parse("https://2chen.moe/a/100").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://sturdychan.help/files/abc.png"
        );
        assert_eq!(files[1].url.as_str(), "https://cdn.example.test/z.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["name"], "Anon & co");
        assert_eq!(k["no"], "100");
        assert_eq!(k["filename"], "my pic");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["hash"], "h1");
        assert_eq!(k["time"], 1_672_628_645);
        assert_eq!(k["date"], "2023-01-02 03:04:05");
        assert_eq!(k["board"], "a");
        assert_eq!(k["thread"], "100");
        assert_eq!(k["title"], "Title & more");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1672628645 my pic.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("sturdychan/a/100 Title & more")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["{board}_{thread}_{no}_{time}",["a","100","100","1672628645"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/100 "));
    }

    #[tokio::test]
    async fn page_without_board_is_an_error() {
        let (origin, _server) = serve(vec![html("<html>nothing</html>")]);
        let http = http("schan", origin);
        let events = extract(
            "schan",
            &http,
            Url::parse("https://schan.help/a/9").unwrap(),
        );
        let results: Vec<_> = futures_util::StreamExt::collect(events).await;
        assert!(results[0].is_err());
    }

    #[tokio::test]
    async fn board_queues_catalog_threads() {
        let catalog = r#"<figure><a href="/a/100">x</a></figure><figure><a href="/a/200">"#;
        let (origin, server) = serve(vec![html(catalog), html(PAGE), html(PAGE)]);
        let http = http("schan", origin);
        let files = media(extract(
            "schan",
            &http,
            Url::parse("https://schan.help/a/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].url.as_str(), "https://schan.help/files/abc.png");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/catalog "));
        assert!(requests[1].starts_with("GET /a/100 "));
        assert!(requests[2].starts_with("GET /a/200 "));
    }
}
