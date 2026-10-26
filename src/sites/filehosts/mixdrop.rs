//! MixDrop files, mirroring gallery_dl/extractor/mixdrop.py: the embed page
//! packs the player script, whose `vfile`/`wurl` give name and file URL.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, ensure_http_scheme, extr, group, nameext_from_name, re, remove_html, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://mixdrop.ag";

static FILE: Route = Route::new(
    "file",
    "{title} ({id}).{extension}",
    &["{category}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "mixdrop",
    name: "MixDrop",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://(?:www\.)?m[1i]xdrop\.(?:com|net|top|ag|bz)/[fe]/([^/?#]+)");
    captures(pattern, url).map(|c| group(&c, 1).to_owned())
}

/// Undo the `p,a,c,k,e,d` packer: each numeric word indexes the keyword list.
fn unpack(page: &str) -> String {
    let mut scan = Scan::new(page);
    let packed = scan.extr("}}return p}('", "'").to_owned();
    let words: Vec<&str> = scan.extr(",'", "'.split(").split('|').collect();
    re!(r"\b\w+\b")
        .replace_all(&packed, |c: &regex::Captures<'_>| {
            let token = &c[0];
            token
                .parse::<usize>()
                .ok()
                .and_then(|i| words.get(i))
                .map_or_else(|| token.to_owned(), |w| (*w).to_owned())
        })
        .into_owned()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported MixDrop URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}/e/{id}"))?).await?;
        let script = unpack(&page);
        let (name, ext) = nameext_from_name(extr(&script, ".vfile=\"", "\""));
        let mut data = Map::new();
        data.insert("filename".into(), name.into());
        data.insert("extension".into(), ext.into());
        data.insert("id".into(), id.as_str().into());
        data.insert(
            "title".into(),
            unescape(&remove_html(extr(&page, "<div class=\"title\">", "</div>"))).into(),
        );
        data.insert(
            "poster".into(),
            ensure_http_scheme(extr(&script, ".poster=\"", "\"")).into(),
        );
        let file = Url::parse(&ensure_http_scheme(extr(&script, ".wurl=\"", "\"")))
            .map_err(|_| error("MixDrop file has no download URL"))?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &FILE,
            vec![Item::new(file, data).ids(id.clone(), id)],
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for raw in [
            "https://mixdrop.ag/f/k0mmklw8axe09e",
            "https://m1xdrop.com/e/k0mmklw8axe09e",
            "https://mixdrop.top/f/k0mmklw8axe09e",
        ] {
            assert_eq!(t(raw).as_deref(), Some("k0mmklw8axe09e"), "{raw}");
        }
        assert_eq!(t("https://mixdrop.ag/x/abc"), None);
    }

    #[tokio::test]
    async fn unpacks_player_script() {
        let page = r#"<div class="title"><b>My &amp; Video</b></div>
            eval(function(p,a,c,k,e,d){while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+c+'\\b','g'),k[c])}}return p}('0.1="//2.3/4.5";0.6="7.5";0.8="//2.3/9.10"',11,11,'MDCore|wurl|s1|mxcontent|video|mp4|vfile|clip|poster|thumb|jpg'.split('|')))"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("mixdrop", root);
        let files = media(extract(
            &http,
            Url::parse("https://mixdrop.ag/f/abc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://s1.mxcontent/video.mp4");
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "clip");
        assert_eq!(k["title"], "My & Video");
        assert_eq!(k["poster"], "https://s1.mxcontent/thumb.jpg");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "My & Video (abc).mp4");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /e/abc "));
    }
}
