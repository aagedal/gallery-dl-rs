//! Single-image hosts, mirroring the `ImagehostImageExtractor` classes in
//! gallery_dl/extractor/imagehosts.py: each host is its own category whose
//! image page names one file, and gallery classes queue their image pages.
//! postimg, from the same upstream module, is extracted by crate::imagehosts.
use super::{Extr, extract_iter, nameext_from_name, nameext_from_url, parse_bytes, parse_int};
use crate::sites::art::ext_from_url;
use crate::sites::http::Body;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

static IMAGE: Route = Route::new(
    "image",
    "{filename}.{extension}",
    &["{category}"],
    "{token}",
);
static GALLERY: Route = Route::new(
    "gallery",
    "{filename}.{extension}",
    &["{category}"],
    "{token}",
);

macro_rules! host {
    ($ident:ident, $host:ident, $category:literal, $name:literal, $routes:expr, $cookies:expr) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(Host::$host, url).is_some(),
            extract: |http, url| extract(Host::$host, http, url),
            routes: $routes,
            api_key: false,
            cookies: $cookies,
        };
    };
}
host!(IMXTO, Imxto, "imxto", "IMX.to", &[&IMAGE, &GALLERY], None);
host!(ACIDIMG, Acidimg, "acidimg", "AcidImg", &[&IMAGE], None);
host!(
    IMAGEVENUE,
    Imagevenue,
    "imagevenue",
    "ImageVenue",
    &[&IMAGE],
    Some("imagevenue.com")
);
host!(
    IMAGETWIST,
    Imagetwist,
    "imagetwist",
    "ImageTwist",
    &[&IMAGE, &GALLERY],
    Some("imagetwist.com")
);
host!(
    IMGADULT,
    Imgadult,
    "imgadult",
    "ImgAdult",
    &[&IMAGE],
    Some("imgadult.com")
);
host!(IMGSPICE, Imgspice, "imgspice", "ImgSpice", &[&IMAGE], None);
host!(
    PIXHOST,
    Pixhost,
    "pixhost",
    "PiXhost",
    &[&IMAGE, &GALLERY],
    Some("pixhost.to")
);
host!(
    TURBOIMAGEHOST,
    Turboimagehost,
    "turboimagehost",
    "TurboImageHost",
    &[&IMAGE, &GALLERY],
    None
);
host!(VIPR, Vipr, "vipr", "Vipr", &[&IMAGE], None);
host!(IMGCLICK, Imgclick, "imgclick", "ImgClick", &[&IMAGE], None);
host!(FAPPIC, Fappic, "fappic", "Fappic", &[&IMAGE], None);
host!(PICSTATE, Picstate, "picstate", "PicState", &[&IMAGE], None);
host!(IMGDRIVE, Imgdrive, "imgdrive", "ImgDrive", &[&IMAGE], None);
host!(IMGTAXI, Imgtaxi, "imgtaxi", "ImgTaxi", &[&IMAGE], None);
host!(
    IMGWALLET,
    Imgwallet,
    "imgwallet",
    "ImgWallet",
    &[&IMAGE],
    None
);
host!(
    SILVERPIC,
    Silverpic,
    "silverpic",
    "SilverPic",
    &[&IMAGE],
    None
);
host!(IMGPV, Imgpv, "imgpv", "ImgPV", &[&IMAGE], None);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Host {
    Imxto,
    Acidimg,
    Imagevenue,
    Imagetwist,
    Imgadult,
    Imgspice,
    Pixhost,
    Turboimagehost,
    Vipr,
    Imgclick,
    Fappic,
    Picstate,
    /// `ImgdriveImageExtractor` takes its category from the host name.
    Imgdrive,
    Imgtaxi,
    Imgwallet,
    Silverpic,
    Imgpv,
}

/// The `_params` form sent as a POST instead of a plain GET.
enum Params {
    None,
    /// `"simple"`: the continue button.
    Simple,
    /// `"complex"`: the XFileSharing `op=view` form for the token.
    Complex,
}

impl Host {
    fn site(self) -> &'static Site {
        match self {
            Self::Imxto => &IMXTO,
            Self::Acidimg => &ACIDIMG,
            Self::Imagevenue => &IMAGEVENUE,
            Self::Imagetwist => &IMAGETWIST,
            Self::Imgadult => &IMGADULT,
            Self::Imgspice => &IMGSPICE,
            Self::Pixhost => &PIXHOST,
            Self::Turboimagehost => &TURBOIMAGEHOST,
            Self::Vipr => &VIPR,
            Self::Imgclick => &IMGCLICK,
            Self::Fappic => &FAPPIC,
            Self::Picstate => &PICSTATE,
            Self::Imgdrive => &IMGDRIVE,
            Self::Imgtaxi => &IMGTAXI,
            Self::Imgwallet => &IMGWALLET,
            Self::Silverpic => &SILVERPIC,
            Self::Imgpv => &IMGPV,
        }
    }

    fn params(self) -> Params {
        match self {
            Self::Imxto | Self::Acidimg => Params::Simple,
            Self::Imgclick | Self::Silverpic => Params::Complex,
            _ => Params::None,
        }
    }

    /// `_cookies` given as a dictionary.
    fn cookies(self) -> &'static [&'static str] {
        match self {
            Self::Imgadult => &["img_i_d=1"],
            Self::Pixhost => &["pixhostads=1", "pixhosttest=1"],
            _ => &[],
        }
    }

    /// `_cookies` as a method: the cookies a plain request of the image page
    /// sets, which upstream caches and sends with the real request.
    fn primes_cookies(self) -> bool {
        matches!(self, Self::Imagevenue | Self::Imagetwist)
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    /// Upstream `page_url` and `token`.
    Image { page: String, token: String },
    /// The gallery `page_url`; `id` is the imagetwist `/p/USER/ID` number.
    Gallery { page: String, id: Option<String> },
}

fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The leading run of `text` whose characters satisfy `accept`.
fn run(text: &str, accept: fn(char) -> bool) -> &str {
    &text[..text.find(|c: char| !accept(c)).unwrap_or(text.len())]
}

/// The first raw path segment of `rest` (upstream `[^/?#]+`), if any.
fn segment(rest: &str) -> Option<&str> {
    Some(run(rest, |c| c != '/')).filter(|s| !s.is_empty())
}

/// The URL after its host, as upstream's patterns see it.
fn tail(url: &Url) -> String {
    let mut out = url.path().to_owned();
    if let Some(query) = url.query() {
        out.push('?');
        out.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// `NAME.html` for `[^/?#]+\.html` (or `\S+\.html` when `any`): the text
/// up to its last `.html` after at least one character.
fn html_name(text: &str, any: bool) -> Option<&str> {
    let text = if any { text } else { run(text, |c| c != '/') };
    let end = text.rfind(".html").filter(|&i| i > 0)? + 5;
    Some(&text[..end])
}

/// `prefix` + `run` + `.html`, the token of `/img-ID.html` paths.
fn img_html<'t>(path: &'t str, prefix: &str, accept: fn(char) -> bool) -> Option<&'t str> {
    let rest = path.strip_prefix(prefix)?;
    let id = run(rest, accept);
    (!id.is_empty() && rest[id.len()..].starts_with(".html")).then_some(id)
}

fn target(host: Host, url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let name = url.host_str()?;
    let bare = name.strip_prefix("www.").unwrap_or(name);
    let path = url.path();
    let image = |page: String, token: &str| {
        Some(Target::Image {
            page,
            token: token.to_owned(),
        })
    };
    match host {
        Host::Imxto => {
            if !matches!(bare, "imx.to" | "img.yt") {
                return None;
            }
            for prefix in ["/i/", "/img-"] {
                if let Some(rest) = path.strip_prefix(prefix) {
                    let token = run(rest, word);
                    if token.is_empty() {
                        return None;
                    }
                    let html = if rest[token.len()..].starts_with(".html") {
                        ".html"
                    } else {
                        ""
                    };
                    let mut page = format!("https://{bare}{prefix}{token}{html}");
                    if prefix == "/img-" {
                        page = page.replace("img.yt", "imx.to");
                    }
                    return image(page, token);
                }
            }
            let id = segment(path.strip_prefix("/g/")?)?;
            (bare == "imx.to").then(|| Target::Gallery {
                page: format!("https://imx.to/g/{id}"),
                id: None,
            })
        }
        Host::Acidimg | Host::Imgadult => {
            let (domain, accept): (_, fn(char) -> bool) = match host {
                Host::Acidimg => ("acidimg.cc", |c| {
                    c.is_ascii_digit() || c.is_ascii_lowercase()
                }),
                _ => ("imgadult.com", |c| {
                    c.is_ascii_hexdigit() && !c.is_ascii_uppercase()
                }),
            };
            if bare != domain {
                return None;
            }
            let token = img_html(path, "/img-", accept)?;
            image(format!("https://{name}/img-{token}.html"), token)
        }
        Host::Imagevenue => {
            let sub = name.strip_suffix(".imagevenue.com")?;
            let numbered = sub
                .strip_prefix("img")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if sub != "www" && !numbered {
                return None;
            }
            let rest = tail(url);
            let rest = &rest[1..];
            let token = if rest.starts_with("view/") || rest.starts_with("img.php?") {
                rest
            } else {
                let id = run(rest, |c| c.is_ascii_uppercase() || c.is_ascii_digit());
                if id.len() < 8 {
                    return None;
                }
                &id[..id.len().min(10)]
            };
            image(format!("https://{name}/{token}"), token)
        }
        Host::Imagetwist => {
            let domain = name
                .strip_prefix("www.")
                .or_else(|| name.strip_prefix("phun."))
                .unwrap_or(name);
            if !matches!(domain, "imagetwist.com" | "imagehaha.com") {
                return None;
            }
            let id = run(&path[1..], |c| c.is_ascii_digit() || c.is_ascii_lowercase());
            if id.len() >= 12 {
                return image(format!("https://{name}/{}", &id[..12]), &id[..12]);
            }
            if let Some(rest) = path.strip_prefix("/p/") {
                let user = segment(rest)?;
                let digits = run(rest[user.len()..].strip_prefix('/')?, |c| {
                    c.is_ascii_digit()
                });
                return (!digits.is_empty()).then(|| Target::Gallery {
                    page: format!("https://{name}/p/{user}/{digits}"),
                    id: Some(digits.to_owned()),
                });
            }
            let query = gallery_query(url.query().filter(|_| path == "/")?)?;
            Some(Target::Gallery {
                page: format!("https://{name}/?{query}"),
                id: None,
            })
        }
        Host::Imgspice | Host::Imgclick | Host::Picstate => {
            let (domain, prefix) = match host {
                Host::Imgspice => ("imgspice.com", "/"),
                Host::Imgclick => ("imgclick.net", "/"),
                _ => ("picstate.com", "/view/full/"),
            };
            if bare != domain {
                return None;
            }
            let token = segment(path.strip_prefix(prefix)?)?;
            image(format!("https://{name}{prefix}{token}"), token)
        }
        Host::Pixhost => {
            if !matches!(bare, "pixhost.to" | "pixhost.org") {
                return None;
            }
            if let Some(id) = path.strip_prefix("/gallery/").and_then(segment) {
                return Some(Target::Gallery {
                    page: format!("https://pixhost.to/gallery/{id}"),
                    id: None,
                });
            }
            let rest = path.strip_prefix("/show/")?;
            let dir = run(rest, |c| c.is_ascii_digit());
            let rest = rest[dir.len()..].strip_prefix('/')?;
            let token = run(rest, |c| c.is_ascii_digit());
            let file = segment(rest)?;
            if dir.is_empty() || token.is_empty() || !file[token.len()..].starts_with('_') {
                return None;
            }
            if file.len() == token.len() + 1 {
                return None;
            }
            image(format!("https://pixhost.to/show/{dir}/{file}"), token)
        }
        Host::Turboimagehost => {
            if bare != "turboimagehost.com" {
                return None;
            }
            if let Some(rest) = path.strip_prefix("/album/") {
                let id = run(rest, |c| c.is_ascii_digit());
                let slug = rest[id.len()..].strip_prefix('/')?;
                let slug = run(slug, |c| c != '/');
                return (!id.is_empty()).then(|| Target::Gallery {
                    page: format!("https://{name}/album/{id}/{slug}"),
                    id: None,
                });
            }
            let rest = path.strip_prefix("/p/")?;
            let id = run(rest, |c| c.is_ascii_digit());
            let file = html_name(rest[id.len()..].strip_prefix('/')?, false)?;
            if id.is_empty() {
                return None;
            }
            image(format!("https://{name}/p/{id}/{file}"), id)
        }
        Host::Vipr => {
            let token = run(&path[1..], word);
            (name == "vipr.im" && !token.is_empty())
                .then(|| image(format!("https://vipr.im/{token}"), token))?
        }
        Host::Fappic => {
            let sub = name.strip_suffix("fappic.com")?;
            let numbered = sub
                .strip_prefix("img")
                .and_then(|n| n.strip_suffix('.'))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if !(sub.is_empty() || sub == "www." || numbered) {
                return None;
            }
            /// `(\w{10,})(?:/|\.)\w+`.
            fn fappic(rest: &str) -> Option<&str> {
                let token = run(rest, word);
                let mut after = rest[token.len()..].chars();
                (token.len() >= 10
                    && matches!(after.next(), Some('/' | '.'))
                    && after.next().is_some_and(word))
                .then_some(token)
            }
            let thumb = path.strip_prefix("/i/").and_then(|rest| {
                let digits = run(rest, |c| c.is_ascii_digit());
                let rest = rest[digits.len()..].strip_prefix('/')?;
                (!digits.is_empty()).then_some(rest).and_then(fappic)
            });
            let token = match thumb {
                Some(token) => token.strip_suffix("_t").unwrap_or(token),
                None => fappic(&path[1..])?,
            };
            image(format!("https://fappic.com/{token}/pic.jpg"), token)
        }
        Host::Imgdrive | Host::Imgtaxi | Host::Imgwallet => {
            let (label, tld) = bare.split_once('.')?;
            let expected = match host {
                Host::Imgdrive => "imgdrive",
                Host::Imgtaxi => "imgtaxi",
                _ => "imgwallet",
            };
            if label != expected || !matches!(tld, "com" | "net") {
                return None;
            }
            let token = img_html(path, "/img-", word)?;
            image(format!("https://{bare}/img-{token}.html"), token)
        }
        Host::Silverpic | Host::Imgpv => {
            let (domains, root): (&[&str], _) = match host {
                Host::Silverpic => (&["silverpic.net", "silverpic.com"], "https://silverpic.net"),
                _ => (&["imgpv.com"], "https://imgpv.com"),
            };
            if !domains.contains(&bare) {
                return None;
            }
            let rest = tail(url);
            let token = run(&rest[1..], |c| c.is_ascii_digit() || c.is_ascii_lowercase());
            let after = rest[1 + token.len()..].strip_prefix('/')?;
            if token.len() < 10 {
                return None;
            }
            let file = html_name(after, true)?;
            image(format!("{root}/{token}/{file}"), token)
        }
    }
}

/// `\?[^#]*\bfld_id=\d+[^#]*&page=\d+`: the query up to its last page
/// number after a folder ID.
fn gallery_query(query: &str) -> Option<&str> {
    let start = query.match_indices("fld_id=").find_map(|(i, _)| {
        let bounded = query[..i].chars().next_back().is_none_or(|c| !word(c));
        let digits = run(&query[i + 7..], |c| c.is_ascii_digit());
        (bounded && !digits.is_empty()).then_some(i + 8)
    })?;
    query
        .match_indices("&page=")
        .filter(|(i, _)| *i >= start)
        .filter_map(|(i, _)| {
            let digits = run(&query[i + 6..], |c| c.is_ascii_digit());
            (!digits.is_empty()).then_some(i + 6 + digits.len())
        })
        .last()
        .map(|end| &query[..end])
}

fn extract(host: Host, http: &Http, url: Url) -> Events<'_> {
    match target(host, &url) {
        Some(Target::Image { page, token }) => crate::sites::once(async move {
            let item = image(host, http, &page, &token).await?;
            Ok(crate::sites::events(crate::sites::post(
                host.site(),
                http,
                &IMAGE,
                item.into_iter().collect(),
            )))
        }),
        Some(Target::Gallery { page, id }) => match host {
            Host::Imxto => imxto_gallery(http, page),
            Host::Imagetwist => imagetwist_gallery(http, page, id),
            Host::Pixhost => pixhost_gallery(http, page),
            _ => turboimagehost_gallery(http, page),
        },
        None => crate::sites::fail(error(format!("unsupported {} URL", host.site().name))),
    }
}

/// `ImagehostImageExtractor.items`: the image page, its file URL and name.
async fn image(host: Host, http: &Http, page_url: &str, token: &str) -> Result<Option<Item>> {
    // ImageVenue redirects `www.` pages to its bare host; site requests
    // follow same-origin redirects only, so ask the bare host directly.
    let api = match host {
        Host::Imagevenue => http.api(&page_url.replacen("://www.", "://", 1))?,
        _ => http.api(page_url)?,
    };
    for cookie in host.cookies() {
        http.set_cookie(&api, &format!("{cookie}; Path=/"));
    }
    if host.primes_cookies() && http.cookie_header(&api).is_none() {
        http.get(&api, &[]).await?;
    }
    let continue_ = "Continue+to+image+...+";
    let page = match host.params() {
        Params::None => http.text(&api).await?,
        Params::Simple => {
            let form = [("imgContinue", continue_)];
            let body = http.post(&api, Body::Form(&form), &[]).await?;
            String::from_utf8_lossy(&body).into_owned()
        }
        Params::Complex => {
            let form = [
                ("op", "view"),
                ("id", token),
                ("pre", "1"),
                ("adb", "1"),
                ("next", continue_),
            ];
            let body = http.post(&api, Body::Form(&form), &[]).await?;
            String::from_utf8_lossy(&body).into_owned()
        }
    };
    let (url, filename) = match info(host, &page) {
        Ok((Some(url), filename)) if !url.is_empty() => (url, filename),
        Ok(_) => return Ok(None),
        Err(()) => return Err(error(format!("{} image not found", host.site().name))),
    };
    let mut data = Map::new();
    match filename.filter(|name| !name.is_empty()) {
        Some(name) => {
            nameext_from_name(&name, &mut data);
            if data["extension"] == "" {
                data.insert("extension".into(), ext_from_url(&url).into());
            }
        }
        None => nameext_from_url(&url, &mut data),
    }
    data.insert("token".into(), token.into());
    data.insert("post_url".into(), page_url.into());
    if host == Host::Pixhost {
        // `self.kwdict["directory"]`: the number before the file name.
        let directory = page_url.rsplit('/').nth(1).unwrap_or_default();
        data.insert("directory".into(), directory.into());
    }
    data.extend(metadata(host, &page)?);
    let url = match url.strip_prefix("http:") {
        Some(rest) => format!("https:{rest}"),
        None => url,
    };
    let file = crate::sites::art::file_url(&url, page_url)
        .ok_or_else(|| error(format!("{} returned an invalid file URL", host.site().name)))?;
    Ok(Some(Item::new(file, data).ids(token, token)))
}

/// `text.extract` with upstream's `(None, pos)` on a miss.
fn find<'t>(text: &'t str, begin: &str, end: &str, pos: usize) -> (Option<&'t str>, usize) {
    match super::extract(text, begin, end, pos) {
        Some((value, next)) => (Some(value), next),
        None => (None, pos),
    }
}

fn unescape(text: Option<&str>) -> Option<String> {
    text.map(crate::sites::unescape)
}

type Info = (Option<String>, Option<String>);

/// `get_info`: the file URL and the name to take the filename from;
/// `Err` is upstream's `not_found()` or a missing page marker.
fn info(host: Host, page: &str) -> std::result::Result<Info, ()> {
    let owned = |text: Option<&str>| text.map(str::to_owned);
    Ok(match host {
        Host::Imxto => {
            let (url, pos) = find(
                page,
                "<div style=\"text-align:center;\"><a href=\"",
                "\"",
                0,
            );
            if url.is_none_or(str::is_empty) {
                return Err(());
            }
            (owned(url), owned(find(page, " title=\"", "\"", pos).0))
        }
        Host::Acidimg => {
            let (mut url, mut pos) = find(page, "<img class='centred' src='", "'", 0);
            if url.is_none_or(str::is_empty) {
                (url, pos) = find(page, "<img class=\"centred\" src=\"", "\"", 0);
                if url.is_none_or(str::is_empty) {
                    return Err(());
                }
            }
            let (mut name, pos) = find(page, "alt='", "'", pos);
            if name.is_none_or(str::is_empty) {
                name = find(page, "alt=\"", "\"", pos).0;
            }
            (owned(url), owned(name))
        }
        Host::Imagevenue => {
            let pos = page.find("class=\"card-body").ok_or(())?;
            let (mut url, mut pos) = find(page, "<img src=\"", "\"", pos);
            if url.ok_or(())?.ends_with("/loader.svg") {
                (url, pos) = find(page, "<img src=\"", "\"", pos);
            }
            (owned(url), unescape(find(page, "alt=\"", "\"", pos).0))
        }
        Host::Imagetwist => {
            let (url, pos) = find(page, "<img src=\"", "\"", 0);
            if url.is_some_and(|url| url.starts_with("/imgs/")) {
                return Err(());
            }
            (owned(url), owned(find(page, " alt=\"", "\"", pos).0))
        }
        Host::Imgadult => {
            let (url, pos) = find(page, "' src='", "'", 0);
            let name = find(page, "alt='", "'", pos).0.map(|name| {
                let (head, sep, rhs) = super::rpartition(name, " image hosted at ImgAdult.com");
                let name = if sep.is_empty() || head.is_empty() {
                    rhs
                } else {
                    head
                };
                crate::sites::unescape(name)
            });
            (owned(url), name)
        }
        Host::Imgspice => {
            let pos = page.find("id=\"imgpreview\"").ok_or(())?;
            let (url, pos) = find(page, "src=\"", "\"", pos);
            (owned(url), unescape(find(page, "alt=\"", "\"", pos).0))
        }
        Host::Pixhost => {
            let (url, pos) = find(page, "class=\"image-img\" src=\"", "\"", 0);
            (owned(url), unescape(find(page, "alt=\"", "\"", pos).0))
        }
        Host::Turboimagehost => {
            let pos = page.find("<img ").ok_or(())?;
            (owned(find(page, "src=\"", "\"", pos).0), None)
        }
        Host::Vipr => {
            let (url, pos) = find(page, "<img src=\"", "\"", 0);
            if !url.is_some_and(|url| url.starts_with('h')) {
                return Err(());
            }
            (owned(url), unescape(find(page, " alt=\"", "\"", pos).0))
        }
        Host::Imgclick | Host::Picstate => {
            let (begin, pos) = match host {
                Host::Imgclick => ("<br><img src=\"", 0),
                _ => (
                    "<img src=\"",
                    page.find(" id=\"image_container\"").ok_or(())?,
                ),
            };
            let (url, pos) = find(page, begin, "\"", pos);
            (owned(url), owned(find(page, "alt=\"", "\"", pos).0))
        }
        Host::Fappic => {
            let (url, pos) = find(page, "<a href=\"#\"><img src=\"", "\"", 0);
            let name = find(page, "alt=\"", "\"", pos).0.unwrap_or_default();
            (owned(url), Some(strip_porn_pic(name).to_owned()))
        }
        Host::Imgdrive | Host::Imgtaxi | Host::Imgwallet => {
            let (title, pos) = find(page, "property=\"og:title\" content=\"", "\"", 0);
            let (image, _) = find(page, "property=\"og:image\" content=\"", "\"", pos);
            let title = title.unwrap_or_default();
            let name = title.rsplitn(3, " | ").last().unwrap_or(title);
            (
                Some(image.ok_or(())?.replace("/small/", "/big/")),
                Some(name.to_owned()),
            )
        }
        Host::Silverpic => {
            let (url, pos) = find(page, "<img src=\"/img/", "\"", 0);
            let url = url.ok_or(())?;
            (
                Some(format!("https://silverpic.net/img/{url}")),
                owned(find(page, "alt=\"", "\"", pos).0),
            )
        }
        Host::Imgpv => {
            let (url, pos) = find(page, "id=\"img-preview\" src=\"", "\"", 0);
            (owned(url), unescape(find(page, "alt=\"", "\"", pos).0))
        }
    })
}

/// `re.sub(r"^Porn[ -]Pic(?:s|ture)[ -]", "", filename)`.
fn strip_porn_pic(name: &str) -> &str {
    let strip = || {
        let rest = name.strip_prefix("Porn")?;
        let rest = rest.strip_prefix([' ', '-'])?.strip_prefix("Pic")?;
        let rest = rest
            .strip_prefix("ture")
            .or_else(|| rest.strip_prefix('s'))?;
        rest.strip_prefix([' ', '-'])
    };
    strip().unwrap_or(name)
}

/// `metadata`: the extra keywords some hosts read from the image page.
fn metadata(host: Host, page: &str) -> Result<Map<String, Value>> {
    let mut data = Map::new();
    match host {
        Host::Imxto => {
            let pos = page
                .find("[ FILESIZE <")
                .ok_or_else(|| error("IMX.to image page has no file size"))?;
            let mut extr = Extr::new(&page[pos..]);
            let mut size = extr.next(">", "</span>").replace(' ', "");
            size.pop();
            let dimensions = extr.next(">", " px</span>");
            let (width, _, height) = super::partition(dimensions, "x");
            data.insert("size".into(), parse_bytes(&size).into());
            data.insert("width".into(), parse_int(width).into());
            data.insert("height".into(), parse_int(height).into());
            data.insert("hash".into(), extr.next(">", "</span>").into());
        }
        Host::Silverpic => {
            let (width, height) = match page.find("<img src=\"/img/") {
                Some(pos) => (
                    find(page, "width=\"", "\"", pos).0,
                    find(page, "height=\"", "\"", pos).0,
                ),
                None => (None, None),
            };
            data.insert("width".into(), parse_int(width.unwrap_or_default()).into());
            data.insert(
                "height".into(),
                parse_int(height.unwrap_or_default()).into(),
            );
        }
        Host::Imgpv => {
            let pos = page.find("class=\"upinfo\">").unwrap_or(0);
            let (date, pos) = find(page, "<b>", "by", pos);
            let (user, _) = find(page, ">", "<", pos);
            let parts: Vec<&str> = date.unwrap_or_default().split_whitespace().collect();
            let date = match parts.as_slice() {
                [day, time, ..] => {
                    let day: String = day.chars().take(10).collect();
                    super::parse_iso(&format!("{day} {time}"))
                }
                _ => None,
            };
            data.insert("date".into(), super::date_value(date));
            data.insert(
                "user".into(),
                crate::sites::unescape(user.unwrap_or_default()).into(),
            );
        }
        _ => {}
    }
    Ok(data)
}

/// Queue image pages that belong to `host`, resolved against `base`.
fn queue_images<'a>(
    http: &'a Http,
    host: Host,
    base: &str,
    links: Vec<String>,
    data: &Map<String, Value>,
) -> Events<'a> {
    let base = Url::parse(base).ok();
    let children = links
        .into_iter()
        .filter_map(|link| base.as_ref()?.join(&link).ok())
        .filter(|url| matches!(target(host, url), Some(Target::Image { .. })))
        .map(|url| (url, data.clone()))
        .collect();
    super::queue_all(http, host.site().category, GALLERY.subcategory, children)
}

/// Upper bound on listing pages for one gallery.
const MAX_PAGES: u32 = 10_000;

/// `ImxtoGalleryExtractor.items`: `/i/` links, page by page until the
/// pagination's last link is disabled.
fn imxto_gallery(http: &Http, page_url: String) -> Events<'_> {
    crate::sites::paginate(
        (1u32, None::<(Map<String, Value>, usize)>),
        move |(num, state)| {
            let page_url = page_url.clone();
            async move {
                if num > MAX_PAGES {
                    return Ok(None);
                }
                let mut url = http.api(&page_url)?;
                if num > 1 {
                    url.query_pairs_mut().append_pair("page", &num.to_string());
                }
                let page = http.text(&url).await?;
                let (data, pos) = match state {
                    Some(state) => state,
                    None => {
                        let (title, pos) = find(&page, "<div class=\"title", "<", 0);
                        let title = super::partition(title.unwrap_or_default(), ">").2;
                        let mut data = Map::new();
                        data.insert("title".into(), crate::sites::unescape(title).trim().into());
                        (data, pos)
                    }
                };
                // Upstream keeps the first page's title position for every page.
                let links = extract_iter(page.get(pos..).unwrap_or_default(), "<a href=", " ")
                    .into_iter()
                    .filter(|url| url.contains("/i/"))
                    .map(|url| url.trim_matches(['"', '\'']).to_owned())
                    .collect();
                let events = queue_images(http, Host::Imxto, &page_url, links, &data);
                let last = !page.contains("class=\"pagination")
                    || page.contains("class=\"disabled\">Last");
                Ok(Some((
                    events,
                    (if last { u32::MAX } else { num + 1 }, Some((data, pos))),
                )))
            }
        },
    )
}

/// `ImagetwistGalleryExtractor.items`: the `gallerys` block of each page,
/// following the `»` link.
fn imagetwist_gallery(http: &Http, page_url: String, id: Option<String>) -> Events<'_> {
    let root = page_url[..page_url[8..].find('/').map_or(page_url.len(), |i| i + 8)].to_owned();
    crate::sites::paginate(
        (Some(page_url), None::<Map<String, Value>>, 0u32),
        move |(next, data, count)| {
            let root = root.clone();
            let id = id.clone();
            async move {
                let Some(url) = next.filter(|_| count < MAX_PAGES) else {
                    return Ok(None);
                };
                let page = http.text(&http.api(&url)?).await?;
                let data = data.unwrap_or_else(|| {
                    let mut extr = Extr::new(&page);
                    let mut data = Map::new();
                    let title = extr.next("page_main_title\">", "<");
                    data.insert("gallery_title".into(), crate::sites::unescape(title).into());
                    let id = id.unwrap_or_else(|| extr.next("&amp;fld_id=", "&").to_owned());
                    data.insert("gallery_id".into(), id.into());
                    data
                });
                let gallery = crate::sites::art::extr(&page, "class=\"gallerys", "</div");
                let links = extract_iter(gallery, " href=\"", "\"")
                    .into_iter()
                    .map(|path| format!("{root}{path}"))
                    .collect();
                let events = queue_images(http, Host::Imagetwist, &root, links, &data);
                let next = page.find("&#187;</a>").map(|pos| {
                    let qs = crate::sites::unescape(super::rextr(&page, "href='", "'", Some(pos)));
                    format!("{root}/{qs}")
                });
                Ok(Some((events, (next, Some(data), count + 1))))
            }
        },
    )
}

/// `PixhostGalleryExtractor.items`: the links of the `images` block.
fn pixhost_gallery(http: &Http, page_url: String) -> Events<'_> {
    crate::sites::once(async move {
        let page = http.text(&http.api(&page_url)?).await?;
        let block = crate::sites::art::extr(&page, "class=\"images\"", "</div>");
        let links = extract_iter(block, "<a href=\"", "\"")
            .into_iter()
            .map(str::to_owned)
            .collect();
        Ok(queue_images(
            http,
            Host::Pixhost,
            &page_url,
            links,
            &Map::new(),
        ))
    })
}

/// `TurboimagehostGalleryExtractor.items`: `?p=N` pages until one has no
/// thumbnails.
fn turboimagehost_gallery(http: &Http, page_url: String) -> Events<'_> {
    crate::sites::paginate(1u32, move |num| {
        let page_url = page_url.clone();
        async move {
            if num > MAX_PAGES {
                return Ok(None);
            }
            let mut url = http.api(&page_url)?;
            url.query_pairs_mut().append_pair("p", &num.to_string());
            let page = http.text(&url).await?;
            if num == 1 && page.contains("Requested gallery don`t exist on our website.") {
                return Err(error("TurboImageHost gallery not found"));
            }
            let links: Vec<String> = extract_iter(&page, "\"><a href=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            if links.is_empty() {
                return Ok(None);
            }
            let events = queue_images(http, Host::Turboimagehost, &page_url, links, &Map::new());
            Ok(Some((events, num + 1)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    fn t(host: Host, raw: &str) -> Option<Target> {
        target(host, &Url::parse(raw).unwrap())
    }

    fn img(page: &str, token: &str) -> Option<Target> {
        Some(Target::Image {
            page: page.into(),
            token: token.into(),
        })
    }

    fn gallery(page: &str, id: Option<&str>) -> Option<Target> {
        Some(Target::Gallery {
            page: page.into(),
            id: id.map(Into::into),
        })
    }

    #[test]
    fn classifies_urls() {
        use Host::*;
        let cases = [
            (
                Imxto,
                "https://imx.to/i/1qdeva",
                img("https://imx.to/i/1qdeva", "1qdeva"),
            ),
            (
                Imxto,
                "https://img.yt/img-57a2050547b97.html",
                img("https://imx.to/img-57a2050547b97.html", "57a2050547b97"),
            ),
            (
                Imxto,
                "https://www.img.yt/i/abc",
                img("https://img.yt/i/abc", "abc"),
            ),
            (
                Imxto,
                "https://imx.to/g/1ab2",
                gallery("https://imx.to/g/1ab2", None),
            ),
            (
                Acidimg,
                "https://www.acidimg.cc/img-5acb6b9de4640.html",
                img(
                    "https://www.acidimg.cc/img-5acb6b9de4640.html",
                    "5acb6b9de4640",
                ),
            ),
            (
                Imagevenue,
                "https://www.imagevenue.com/ME13LS07",
                img("https://www.imagevenue.com/ME13LS07", "ME13LS07"),
            ),
            (
                Imagevenue,
                "https://www.imagevenue.com/ME123456789",
                img("https://www.imagevenue.com/ME12345678", "ME12345678"),
            ),
            (
                Imagevenue,
                "https://img28116.imagevenue.com/img.php?image=th_52709_test_122_64lo.jpg",
                img(
                    "https://img28116.imagevenue.com/img.php?image=th_52709_test_122_64lo.jpg",
                    "img.php?image=th_52709_test_122_64lo.jpg",
                ),
            ),
            (
                Imagetwist,
                "https://phun.imagehaha.com/f1i2s4vhvbrq/test.png",
                img("https://phun.imagehaha.com/f1i2s4vhvbrq", "f1i2s4vhvbrq"),
            ),
            (
                Imagetwist,
                "https://imagetwist.com/p/gdldev/747223/digits",
                gallery("https://imagetwist.com/p/gdldev/747223", Some("747223")),
            ),
            (
                Imagetwist,
                "https://imagetwist.com/?op=user_public&fld_id=747223&page=3&x=1",
                gallery(
                    "https://imagetwist.com/?op=user_public&fld_id=747223&page=3",
                    None,
                ),
            ),
            (
                Imgadult,
                "https://imgadult.com/img-5e0aa4c1d2e2a.html",
                img(
                    "https://imgadult.com/img-5e0aa4c1d2e2a.html",
                    "5e0aa4c1d2e2a",
                ),
            ),
            (
                Imgspice,
                "https://imgspice.com/nwfwtpyog50y/test.png.html",
                img("https://imgspice.com/nwfwtpyog50y", "nwfwtpyog50y"),
            ),
            (
                Pixhost,
                "http://www.pixhost.org/show/190/130327671_test-.png",
                img(
                    "https://pixhost.to/show/190/130327671_test-.png",
                    "130327671",
                ),
            ),
            (
                Pixhost,
                "https://pixhost.to/gallery/jSMFq",
                gallery("https://pixhost.to/gallery/jSMFq", None),
            ),
            (
                Turboimagehost,
                "https://www.turboimagehost.com/p/39078423/test--.png.html?x",
                img(
                    "https://www.turboimagehost.com/p/39078423/test--.png.html",
                    "39078423",
                ),
            ),
            (
                Turboimagehost,
                "https://turboimagehost.com/album/12345/",
                gallery("https://turboimagehost.com/album/12345/", None),
            ),
            (
                Vipr,
                "https://vipr.im/kcd5jcuhgs3v.html",
                img("https://vipr.im/kcd5jcuhgs3v", "kcd5jcuhgs3v"),
            ),
            (
                Imgclick,
                "http://imgclick.net/4tbrre1oxew9/test-_-_.png.html",
                img("https://imgclick.net/4tbrre1oxew9", "4tbrre1oxew9"),
            ),
            (
                Fappic,
                "https://img34.fappic.com/i/00044/b8vkdr3xi0fv_t.jpg",
                img("https://fappic.com/b8vkdr3xi0fv/pic.jpg", "b8vkdr3xi0fv"),
            ),
            (
                Fappic,
                "https://fappic.com/98wxqcklyh8k_t/Test.jpg",
                img(
                    "https://fappic.com/98wxqcklyh8k_t/pic.jpg",
                    "98wxqcklyh8k_t",
                ),
            ),
            (
                Picstate,
                "https://picstate.com/view/full/23416694_tcbwo",
                img(
                    "https://picstate.com/view/full/23416694_tcbwo",
                    "23416694_tcbwo",
                ),
            ),
            (
                Imgtaxi,
                "https://www.imgtaxi.com/img-5f3f1d1c26f7c.html",
                img(
                    "https://imgtaxi.com/img-5f3f1d1c26f7c.html",
                    "5f3f1d1c26f7c",
                ),
            ),
            (
                Silverpic,
                "https://www.silverpic.com/a1b2c3d4f5g6/a/NAME.EXT.html?x",
                img(
                    "https://silverpic.net/a1b2c3d4f5g6/a/NAME.EXT.html",
                    "a1b2c3d4f5g6",
                ),
            ),
            (
                Imgpv,
                "https://www.imgpv.com/a1b2c3d4f5g6/NAME.EXT.html",
                img(
                    "https://imgpv.com/a1b2c3d4f5g6/NAME.EXT.html",
                    "a1b2c3d4f5g6",
                ),
            ),
        ];
        for (host, raw, expected) in cases {
            assert_eq!(t(host, raw), expected, "{raw}");
        }
        for (host, raw) in [
            (Imxto, "https://imx.to/x/abc"),
            (Imxto, "https://img.yt/g/abc"),
            (Acidimg, "https://acidimg.cc/img-ABC.html"),
            (Acidimg, "https://acidimg.cc/img-abc"),
            (Imagevenue, "https://imagevenue.com/ME13LS07"),
            (Imagevenue, "https://www.imagevenue.com/ME13LS0"),
            (Imagevenue, "https://img.imagevenue.com/ME13LS07"),
            (Imagetwist, "https://imagetwist.com/f1i2s4vhvbr"),
            (Imagetwist, "https://imagetwist.com/?fld_id=1"),
            (Imagetwist, "https://imagetwist.com/?xfld_id=1&page=2"),
            (Imagetwist, "https://evil.imagetwist.com/f1i2s4vhvbrq"),
            (Pixhost, "https://pixhost.to/show/190/x_test.png"),
            (Pixhost, "https://pixhost.to/show/190/130327671_"),
            (Turboimagehost, "https://turboimagehost.com/p/1/.html"),
            (Turboimagehost, "https://turboimagehost.com/album/12345"),
            (Vipr, "https://www.vipr.im/kcd5jcuhgs3v.html"),
            (Fappic, "https://fappic.com/short/x.jpg"),
            (Fappic, "https://fappic.com/98wxqcklyh8k_t"),
            (Fappic, "https://evilfappic.com/98wxqcklyh8k/x.jpg"),
            (Imgdrive, "https://imgtaxi.com/img-5f3f1d1c26f7c.html"),
            (Imgdrive, "https://imgdrive.org/img-5f3f1d1c26f7c.html"),
            (Silverpic, "https://silverpic.net/a1b2c3d4f/NAME.EXT.html"),
            (Imgpv, "https://imgpv.com/a1b2c3d4f5g6/NAME.EXT"),
            (Imgspice, "https://imgspice.com.evil.test/abc"),
            (Imgclick, "https://imgclick.net:8443/abc"),
            (Picstate, "https://u:p@picstate.com/view/full/1"),
        ] {
            assert_eq!(t(host, raw), None, "{raw}");
        }
        assert!((IMGTAXI.matches)(
            &Url::parse("https://imgtaxi.com/img-abc.html").unwrap()
        ));
    }

    #[test]
    fn strips_fappic_prefixes() {
        assert_eq!(strip_porn_pic("Porn-Pics-Test"), "Test");
        assert_eq!(strip_porn_pic("Porn-Pic-Test"), "Porn-Pic-Test");
        assert_eq!(strip_porn_pic("Porn Pictures Test"), "Porn Pictures Test");
        assert_eq!(strip_porn_pic("Porn Picture-Test"), "Test");
        assert_eq!(strip_porn_pic("Test"), "Test");
    }

    #[tokio::test]
    async fn imxto_posts_the_continue_form() {
        let page = "<div style=\"text-align:center;\"><a href=\"http://i.imx.to/i/2024/a.jpg\" title=\"Holiday.JPG\">\
            [ FILESIZE <span>2.5 MB</span> <span>800x600 px</span> <span>abcdef</span>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("imxto", root);
        let files = media(extract(
            Host::Imxto,
            &http,
            Url::parse("https://imx.to/i/1qdeva").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://i.imx.to/i/2024/a.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "Holiday");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["token"], "1qdeva");
        assert_eq!(k["post_url"], "https://imx.to/i/1qdeva");
        assert_eq!(k["size"], 2_621_440);
        assert_eq!(k["width"], 800);
        assert_eq!(k["height"], 600);
        assert_eq!(k["hash"], "abcdef");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Holiday.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imxto")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap(),
            ("imxto", r#"["{token}",["1qdeva"]]"#.to_owned())
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /i/1qdeva "));
        assert!(requests[0].ends_with("imgContinue=Continue%2Bto%2Bimage%2B...%2B"));
    }

    #[tokio::test]
    async fn complex_form_and_metadata() {
        let page = "<img src=\"/img/ab/c.png\" width=\"640\" height=\"480\" alt=\"Pic.png\">";
        let (root, server) = serve(vec![html(page)]);
        let http = http("silverpic", root);
        let files = media(extract(
            Host::Silverpic,
            &http,
            Url::parse("https://silverpic.com/a1b2c3d4f5g6/Pic.png.html").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://silverpic.net/img/ab/c.png");
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "Pic");
        assert_eq!(k["width"], 640);
        assert_eq!(k["height"], 480);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /a1b2c3d4f5g6/Pic.png.html "));
        assert!(
            requests[0].ends_with(
                "op=view&id=a1b2c3d4f5g6&pre=1&adb=1&next=Continue%2Bto%2Bimage%2B...%2B"
            )
        );
    }

    #[tokio::test]
    async fn imagetwist_primes_cookies() {
        let first = crate::sites::testing::response(
            "200 OK\r\nSet-Cookie: session=1; Path=/",
            "text/html",
            b"",
        );
        let page = "<img src=\"https://i.imagetwist.com/i/x/f1i2s4vhvbrq.png/test.png\" class=\"pic\" alt=\"test.png\">";
        let (root, server) = serve(vec![first, html(page), html("<img src=\"/imgs/404.png\">")]);
        let http = http("imagetwist", root);
        let files = media(extract(
            Host::Imagetwist,
            &http,
            Url::parse("https://imagetwist.com/f1i2s4vhvbrq/test.png").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["filename"], "test");
        let missing = extract(
            Host::Imagetwist,
            &http,
            Url::parse("https://imagetwist.com/aaaaaaaaaaaa").unwrap(),
        );
        assert!(crate::sites::testing::fails(missing).await);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /f1i2s4vhvbrq "));
        assert!(!requests[0].contains("cookie:"));
        assert!(requests[1].contains("cookie: session=1"));
        assert!(requests[2].starts_with("GET /aaaaaaaaaaaa "));
    }

    #[tokio::test]
    async fn pixhost_sends_cookies_and_directory() {
        let page = "<img class=\"image-img\" src=\"https://img1.pixhost.to/images/190/130327671_test-.png\" alt=\"test &amp;.png\">";
        let (root, server) = serve(vec![html(page)]);
        let http = http("pixhost", root);
        let files = media(extract(
            Host::Pixhost,
            &http,
            Url::parse("https://pixhost.to/show/190/130327671_test-.png").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["directory"], "190");
        assert_eq!(k["filename"], "test &");
        assert_eq!(k["token"], "130327671");
        let requests = server.join().unwrap();
        assert!(requests[0].contains("cookie: pixhostads=1; pixhosttest=1"));
    }

    #[tokio::test]
    async fn names_fall_back_to_the_url() {
        let page = "<meta property=\"og:title\" content=\"a | b | c | d\">\
            <meta property=\"og:image\" content=\"https://img.imgtaxi.com/small/x/abc.jpg\">";
        let (root, server) = serve(vec![html(page), html("<img src=\"x.gif\">")]);
        let http = http("imgtaxi", root);
        let files = media(extract(
            Host::Imgtaxi,
            &http,
            Url::parse("https://imgtaxi.com/img-abc.html").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://img.imgtaxi.com/big/x/abc.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "imgtaxi");
        assert_eq!(k["filename"], "a | b");
        assert_eq!(k["extension"], "jpg");
        let files = media(extract(
            Host::Turboimagehost,
            &http,
            Url::parse("https://turboimagehost.com/p/1/a.png.html").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["filename"], "x");
        assert_eq!(
            files[0].url.as_str(),
            "https://turboimagehost.com/p/1/x.gif"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn imxto_gallery_pages_until_last() {
        let first = "<div class=\"title\"> Trip &amp; Co </div><a href='https://imx.to/i/aaa' x>\
            <a href=\"https://imx.to/g/other\" x><a href=https://evil.test/i/bbb x>\
            <ul class=\"pagination\">";
        let second = "<div class=\"title\"> Trip &amp; Co </div><a href=\"https://imx.to/i/ccc\" x>\
            <ul class=\"pagination\"><li class=\"disabled\">Last";
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("imxto", root);
        let children = queued(extract(
            Host::Imxto,
            &http,
            Url::parse("https://imx.to/g/1ab2").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "gallery");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /g/1ab2 "));
        assert!(requests[1].starts_with("GET /g/1ab2?page=2 "));
    }

    #[tokio::test]
    async fn imagetwist_gallery_follows_next_links() {
        let first = "<div class=\"page_main_title\">My &amp; Pics</div>\
            <div class=\"gallerys\"><a href=\"/aaaaaaaaaaaa/a.jpg\">x</a></div>\
            <a href='?op=user_public&amp;fld_id=7&amp;page=2'>&#187;</a>";
        let second = "<div class=\"gallerys\"><a href=\"/bbbbbbbbbbbb/b.jpg\">x</a></div>";
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("imagetwist", root);
        let children = queued(extract(
            Host::Imagetwist,
            &http,
            Url::parse("https://imagetwist.com/p/user/7/x").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /p/user/7 "));
        assert!(requests[1].starts_with("GET /?op=user_public&fld_id=7&page=2 "));
    }

    #[tokio::test]
    async fn pixhost_and_turboimagehost_galleries() {
        let pixhost = "<div class=\"images\"><a href=\"https://pixhost.to/show/1/2_a.jpg\">\
            <a href=\"https://pixhost.to/gallery/x\"></div>";
        let turbo = "<td class=\"t\"><a href=\"https://www.turboimagehost.com/p/5/a.jpg.html\">";
        let (root, server) = serve(vec![html(pixhost), html(turbo), html("<p>end</p>")]);
        let http = http("pixhost", root);
        let children = queued(extract(
            Host::Pixhost,
            &http,
            Url::parse("https://pixhost.to/gallery/x").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        let children = queued(extract(
            Host::Turboimagehost,
            &http,
            Url::parse("https://turboimagehost.com/album/9/name").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].category(), "turboimagehost");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /album/9/name?p=1 "));
        assert!(requests[2].starts_with("GET /album/9/name?p=2 "));
    }
}
