//! Mixdrop files, mirroring gallery_dl/extractor/mixdrop.py: the embed
//! page's packed script, once its words are substituted, names the file,
//! its poster and its download URL.
use crate::sites::art::extr;
use crate::sites::manga::text::ensure_http_scheme;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::Map;
use url::Url;

pub static SITE: Site = Site {
    category: "mixdrop",
    name: "Mixdrop",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE],
    api_key: false,
    cookies: None,
};

static FILE: Route = Route::new(
    "file",
    "{title} ({id}).{extension}",
    &["{category}"],
    "{id}",
);

const ROOT: &str = "https://mixdrop.ag";

/// `(?:www\.)?m[1i]xdrop\.(?:com|net|top|ag|bz)/[fe]/([^/?#]+)`.
fn target(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let (name, tld) = host.split_once('.')?;
    if !crate::sites::plain(url)
        || !matches!(name, "mixdrop" | "m1xdrop")
        || !matches!(tld, "com" | "net" | "top" | "ag" | "bz")
    {
        return None;
    }
    match crate::sites::art::raw_segments(url).as_slice() {
        [kind, id, ..] if matches!(*kind, "f" | "e") && url.path()[3..].starts_with(*id) => {
            Some((*id).to_owned())
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported Mixdrop URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}/e/{id}"))?).await?;
        let item = parse(&page, &id)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &FILE,
            vec![item],
        )))
    })
}

/// Substitute each `\b\w+\b` of the packed script with the word list entry
/// its decimal value indexes, as upstream does with `int()`.
fn unpack(page: &str) -> Result<String> {
    let invalid = || error("Mixdrop page has no packed player script");
    let (packed, pos) = super::extract(page, "}}return p}('", "'", 0).ok_or_else(invalid)?;
    let words: Vec<&str> = super::extract(page, ",'", "'.split(", pos)
        .ok_or_else(invalid)?
        .0
        .split('|')
        .collect();
    let mut out = String::with_capacity(packed.len());
    let mut rest = packed;
    while let Some(start) = rest.find(crate::sites::media::word_char) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let len = tail
            .find(|c: char| !crate::sites::media::word_char(c))
            .unwrap_or(tail.len());
        let word = tail[..len]
            .parse::<usize>()
            .ok()
            .and_then(|index| words.get(index))
            .ok_or_else(invalid)?;
        out.push_str(word);
        rest = &tail[len..];
    }
    out.push_str(rest);
    Ok(out)
}

fn parse(page: &str, id: &str) -> Result<Item> {
    let script = unpack(page)?;
    let mut data = Map::new();
    super::nameext_from_name(extr(&script, ".vfile=\"", "\""), &mut data);
    let title = crate::sites::unescape(&crate::sites::remove_html(extr(
        page,
        "<div class=\"title\">",
        "</div>",
    )));
    data.insert("id".into(), id.into());
    data.insert("title".into(), title.clone().into());
    data.insert(
        "poster".into(),
        ensure_http_scheme(extr(&script, ".poster=\"", "\"")).into(),
    );
    let url = ensure_http_scheme(extr(&script, ".wurl=\"", "\""));
    let file = super::web_url(&url).ok_or_else(|| error("Mixdrop file has no download URL"))?;
    Ok(Item::new(file, data).ids(id, id).caption(Some(title)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://mixdrop.ag/f/abc123/x").as_deref(),
            Some("abc123")
        );
        assert_eq!(t("https://www.m1xdrop.bz/e/abc").as_deref(), Some("abc"));
        for raw in [
            "https://mixdrop.co/f/abc",
            "https://mixdrop.ag/x/abc",
            "https://mixdrop.ag//f/abc",
            "https://cdn.mixdrop.ag/f/abc",
            "https://mixdrop.ag.evil.test/f/abc",
            "https://mixdrop.ag:8080/f/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = "<div class=\"title\"><b>My &amp; Clip</b></div>\
        <script>eval(function(p,a,c,k,e,d){while(c--)if(k[c])p=p.replace(new RegExp('\\\\b'+c.toString(a)+'\\\\b','g'),k[c]);return p}}return p}('0.1=\"//2.3/4.5\";0.6=\"//2.3/7.8\";0.9=\"10.5\";',11,11,'MDCore|wurl|a|mxcontent|v|mp4|poster|thumb|jpg|vfile|Clip_Name'.split('|'),0,{}))</script>";

    #[test]
    fn unpacks_the_player_script() {
        assert_eq!(
            unpack(PAGE).unwrap(),
            "MDCore.wurl=\"//a.mxcontent/v.mp4\";MDCore.poster=\"//a.mxcontent/thumb.jpg\";MDCore.vfile=\"Clip_Name.mp4\";"
        );
        assert!(unpack("<p>gone</p>").is_err());
    }

    #[tokio::test]
    async fn extracts_file() {
        let (root, server) = serve(vec![html(PAGE)]);
        let http = http("mixdrop", root);
        let files = media(extract(
            &http,
            Url::parse("https://mixdrop.ag/f/abc123").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://a.mxcontent/v.mp4");
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "My & Clip");
        assert_eq!(k["poster"], "https://a.mxcontent/thumb.jpg");
        assert_eq!(k["filename"], "Clip_Name");
        assert_eq!(k["extension"], "mp4");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "My & Clip (abc123).mp4");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("mixdrop")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["abc123"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /e/abc123 "));
    }
}
