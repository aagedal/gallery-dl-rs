//! Single-image hosts and their galleries, mirroring the
//! `ImagehostImageExtractor` family in gallery_dl/extractor/imagehosts.py
//! (imx.to, acidimg, imagevenue, imagetwist, imgadult, imgspice, pixhost,
//! turboimagehost, vipr, imgclick, fappic, picstate, imgdrive/imgtaxi/
//! imgwallet, silverpic and imgpv). postimg lives in `crate::imagehosts`.
use crate::error;
use crate::sites::gdl::{
    Scan, date, ext_from_url, extr, extract_iter, group, nameext_from_name, nameext_into,
    parse_bytes, parse_int, parse_iso, partition, re, rextr, rpartition, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
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

/// The request body upstream posts to image pages.
#[derive(Clone, Copy)]
enum Params {
    None,
    /// `imgContinue`.
    Simple,
    /// The XFileSharing `op=view` form.
    Complex,
}

/// Cookies sent with the image page request.
#[derive(Clone, Copy)]
enum Cookies {
    None,
    Fixed(&'static [(&'static str, &'static str)]),
    /// Whatever a first GET of the page sets.
    FromPage,
}

type Info = fn(&str) -> crate::Result<(String, Option<String>)>;
type Meta = fn(&str) -> Map<String, Value>;

struct Host {
    category: &'static str,
    /// Anchored URL pattern: group 1 is the page URL part, group 2 the token.
    pattern: &'static str,
    root: Option<&'static str>,
    params: Params,
    cookies: Cookies,
    info: Info,
    meta: Option<Meta>,
}

fn not_found() -> crate::Error {
    error("imagehost image not found")
}

macro_rules! host {
    ($site:ident, $host:ident, $category:literal, $name:literal, $gallery:expr, {$($field:tt)*}) => {
        static $host: Host = Host { category: $category, $($field)* };
        pub static $site: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$host, $gallery, url).is_some(),
            extract: |http, url| extract(&$host, &$site, $gallery, http, url),
            routes: &[&IMAGE, &GALLERY],
            api_key: false,
            cookies: None,
        };
    };
}

/// A gallery pattern and the function that queues its images.
type GalleryFn = for<'a> fn(&'a Http, &'static Site, regex::Captures<'_>, Url) -> Events<'a>;
type GallerySpec = Option<(&'static str, GalleryFn)>;

host!(IMXTO, IMXTO_HOST, "imxto", "imx.to", Some((r"^https?://(?:www\.)?(imx\.to/g/([^/?#]+))", imxto_gallery as GalleryFn)), {
    pattern: r"^https?://(?:www\.)?((?:imx\.to|img\.yt)/(?:i/|img-)(\w+)(\.html)?)",
    root: None, params: Params::Simple, cookies: Cookies::None, info: imxto_info, meta: Some(imxto_meta),
});
host!(ACIDIMG, ACIDIMG_HOST, "acidimg", "AcidImg", None, {
    pattern: r"^https?://((?:www\.)?acidimg\.cc/img-([a-z0-9]+)\.html)",
    root: None, params: Params::Simple, cookies: Cookies::None, info: acidimg_info, meta: None,
});
host!(IMAGEVENUE, IMAGEVENUE_HOST, "imagevenue", "ImageVenue", None, {
    pattern: r"^https?://((?:www|img\d+)\.imagevenue\.com/([A-Z0-9]{8,10}|view/.*|img\.php\?.*))",
    root: None, params: Params::None, cookies: Cookies::FromPage, info: imagevenue_info, meta: None,
});
host!(IMAGETWIST, IMAGETWIST_HOST, "imagetwist", "ImageTwist", Some((
    r"^https?://((?:www\.|phun\.)?image(?:twist|haha)\.com/(p/[^/?#]+/(\d+)|\?[^#]*\bfld_id=\d+[^#]*&page=\d+))",
    imagetwist_gallery as GalleryFn,
)), {
    pattern: r"^https?://((?:www\.|phun\.)?image(?:twist|haha)\.com/([a-z0-9]{12}))",
    root: None, params: Params::None, cookies: Cookies::FromPage, info: imagetwist_info, meta: None,
});
host!(IMGADULT, IMGADULT_HOST, "imgadult", "ImgAdult", None, {
    pattern: r"^https?://((?:www\.)?imgadult\.com/img-([0-9a-f]+)\.html)",
    root: None, params: Params::None, cookies: Cookies::Fixed(&[("img_i_d", "1")]), info: imgadult_info, meta: None,
});
host!(IMGSPICE, IMGSPICE_HOST, "imgspice", "ImgSpice", None, {
    pattern: r"^https?://((?:www\.)?imgspice\.com/([^/?#]+))",
    root: None, params: Params::None, cookies: Cookies::None, info: imgspice_info, meta: None,
});
host!(PIXHOST, PIXHOST_HOST, "pixhost", "Pixhost", Some((r"^https?://(?:www\.)?pixhost\.(?:to|org)(/gallery/([^/?#]+))", pixhost_gallery as GalleryFn)), {
    pattern: r"^https?://(?:www\.)?pixhost\.(?:to|org)(/show/\d+/(\d+)_[^/?#]+)",
    root: Some("https://pixhost.to"), params: Params::None,
    cookies: Cookies::Fixed(&[("pixhostads", "1"), ("pixhosttest", "1")]), info: pixhost_info, meta: None,
});
host!(TURBOIMAGEHOST, TURBOIMAGEHOST_HOST, "turboimagehost", "TurboImageHost", Some((
    r"^https?://((?:www\.)?turboimagehost\.com/album/(\d+)/([^/?#]*))",
    turbo_gallery as GalleryFn,
)), {
    pattern: r"^https?://((?:www\.)?turboimagehost\.com/p/(\d+)/[^/?#]+\.html)",
    root: None, params: Params::None, cookies: Cookies::None, info: turbo_info, meta: None,
});
host!(VIPR, VIPR_HOST, "vipr", "Vipr", None, {
    pattern: r"^https?://(vipr\.im/(\w+))",
    root: None, params: Params::None, cookies: Cookies::None, info: vipr_info, meta: None,
});
host!(IMGCLICK, IMGCLICK_HOST, "imgclick", "ImgClick", None, {
    pattern: r"^https?://((?:www\.)?imgclick\.net/([^/?#]+))",
    root: None, params: Params::Complex, cookies: Cookies::None, info: imgclick_info, meta: None,
});
host!(FAPPIC, FAPPIC_HOST, "fappic", "Fappic", None, {
    pattern: r"^https?://(?:www\.|img\d+\.)?fappic\.com/(?:i/\d+/())?(\w{10,})(?:/|\.)\w+",
    root: None, params: Params::None, cookies: Cookies::None, info: fappic_info, meta: None,
});
host!(PICSTATE, PICSTATE_HOST, "picstate", "PicState", None, {
    pattern: r"^https?://((?:www\.)?picstate\.com/view/full/([^/?#]+))",
    root: None, params: Params::None, cookies: Cookies::None, info: picstate_info, meta: None,
});
host!(IMGDRIVE, IMGDRIVE_HOST, "imgdrive", "ImgDrive", None, {
    pattern: r"^https?://(?:www\.)?(imgdrive\.(?:com|net)/img-(\w+)\.html)",
    root: None, params: Params::None, cookies: Cookies::None, info: imgdrive_info, meta: None,
});
host!(IMGTAXI, IMGTAXI_HOST, "imgtaxi", "ImgTaxi", None, {
    pattern: r"^https?://(?:www\.)?(imgtaxi\.(?:com|net)/img-(\w+)\.html)",
    root: None, params: Params::None, cookies: Cookies::None, info: imgdrive_info, meta: None,
});
host!(IMGWALLET, IMGWALLET_HOST, "imgwallet", "ImgWallet", None, {
    pattern: r"^https?://(?:www\.)?(imgwallet\.(?:com|net)/img-(\w+)\.html)",
    root: None, params: Params::None, cookies: Cookies::None, info: imgdrive_info, meta: None,
});
host!(SILVERPIC, SILVERPIC_HOST, "silverpic", "SilverPic", None, {
    pattern: r"^https?://(?:www\.)?silverpic\.(?:net|com)(/([a-z0-9]{10,})/\S+\.html)",
    root: Some("https://silverpic.net"), params: Params::Complex, cookies: Cookies::None,
    info: silverpic_info, meta: Some(silverpic_meta),
});
host!(IMGPV, IMGPV_HOST, "imgpv", "ImgPV", None, {
    pattern: r"^https?://(?:www\.)?imgpv\.com(/([a-z0-9]{10,})/\S+\.html)",
    root: Some("https://imgpv.com"), params: Params::None, cookies: Cookies::None, info: imgpv_info, meta: Some(imgpv_meta),
});

fn compiled(pattern: &'static str) -> regex::Regex {
    // Patterns are static; compile per call site through a small cache.
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<&'static str, regex::Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(pattern)
        .or_insert_with(|| regex::Regex::new(pattern).expect("static imagehost pattern"))
        .clone()
}

enum Target<'u> {
    Image(regex::Captures<'u>),
    Gallery(regex::Captures<'u>, GalleryFn),
}

fn target<'u>(host: &Host, gallery: GallerySpec, url: &'u Url) -> Option<Target<'u>> {
    if !crate::sites::plain(url) {
        return None;
    }
    if let Some((pattern, handler)) = gallery
        && let Some(c) = compiled(pattern).captures(url.as_str())
    {
        return Some(Target::Gallery(c, handler));
    }
    compiled(host.pattern)
        .captures(url.as_str())
        .map(Target::Image)
}

fn extract<'a>(
    host: &'static Host,
    site: &'static Site,
    gallery: GallerySpec,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let (page_url, token) = match target(host, gallery, &url) {
        Some(Target::Gallery(caps, handler)) => return handler(http, site, caps, url.clone()),
        Some(Target::Image(caps)) => image_page(host, &caps),
        None => return crate::sites::fail(error("unsupported image host URL")),
    };
    crate::sites::once(async move {
        let items = image(http, host, &page_url, &token).await?;
        Ok(crate::sites::events(crate::sites::post(
            site, http, &IMAGE, items,
        )))
    })
}

/// `page_url` and `token` for a matched image URL.
fn image_page(host: &Host, caps: &regex::Captures<'_>) -> (String, String) {
    if host.category == "fappic" {
        let mut token = group(caps, 2).to_owned();
        if caps.get(1).is_some() && token.ends_with("_t") {
            token.truncate(token.len() - 2);
        }
        return (format!("https://fappic.com/{token}/pic.jpg"), token);
    }
    let mut page_url = format!("{}{}", host.root.unwrap_or("https://"), group(caps, 1));
    if host.category == "imxto" && page_url.contains("/img-") {
        page_url = page_url.replace("img.yt", "imx.to");
    }
    (page_url, group(caps, 2).to_owned())
}

/// `ImagehostImageExtractor.items` for one image page.
async fn image(http: &Http, host: &Host, page_url: &str, token: &str) -> crate::Result<Vec<Item>> {
    let url = http.api(page_url)?;
    match host.cookies {
        Cookies::None => {}
        Cookies::Fixed(cookies) => {
            let domain = url.host_str().unwrap_or_default().to_owned();
            for (name, value) in cookies {
                http.set_cookie(&url, &format!("{name}={value}; Domain={domain}; Path=/"));
            }
        }
        Cookies::FromPage => {
            http.get(&url, &[]).await?;
        }
    }
    let continue_label = "Continue+to+image+...+";
    let page = match host.params {
        Params::None => http.text(&url).await?,
        Params::Simple => {
            http.post_form_text(&url, &[("imgContinue", continue_label)], &[])
                .await?
        }
        Params::Complex => {
            let form = [
                ("op", "view"),
                ("id", token),
                ("pre", "1"),
                ("adb", "1"),
                ("next", continue_label),
            ];
            http.post_form_text(&url, &form, &[]).await?
        }
    };
    let (raw, filename) = (host.info)(&page)?;
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut data = Map::new();
    match filename.filter(|f| !f.is_empty()) {
        Some(filename) => {
            let (name, mut ext) = nameext_from_name(&filename);
            if ext.is_empty() {
                ext = ext_from_url(&raw);
            }
            data.insert("filename".into(), name.into());
            data.insert("extension".into(), ext.into());
        }
        None => nameext_into(&raw, &mut data),
    }
    data.insert("token".into(), token.into());
    data.insert("post_url".into(), page_url.into());
    if let Some(meta) = host.meta {
        data.extend(meta(&page));
    }
    if host.category == "pixhost" {
        let directory = page_url.rsplit('/').nth(1).unwrap_or_default();
        data.insert("directory".into(), directory.into());
    }
    let raw = match raw.strip_prefix("http:") {
        Some(rest) => format!("https:{rest}"),
        None => raw,
    };
    let file = Url::parse(&raw).map_err(|_| error("invalid image host file URL"))?;
    Ok(vec![Item::new(file, data).ids(token, token)])
}

fn found(url: &str) -> crate::Result<&str> {
    if url.is_empty() {
        Err(not_found())
    } else {
        Ok(url)
    }
}

fn imxto_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = found(scan.extr("<div style=\"text-align:center;\"><a href=\"", "\""))?.to_owned();
    let filename = scan
        .find(" title=\"", "\"")
        .filter(|f| !f.is_empty())
        .map(str::to_owned);
    Ok((url, filename))
}

fn imxto_meta(page: &str) -> Map<String, Value> {
    let mut data = Map::new();
    let Some(start) = page.find("[ FILESIZE <") else {
        return data;
    };
    let mut scan = Scan::at(page, start);
    let size = scan.extr(">", "</span>").replace(' ', "");
    let size = size.get(..size.len().saturating_sub(1)).unwrap_or_default();
    let (width, _, height) = partition(scan.extr(">", " px</span>"), "x");
    let (width, height) = (parse_int(width), parse_int(height));
    data.insert("size".into(), parse_bytes(size).into());
    data.insert("width".into(), width.into());
    data.insert("height".into(), height.into());
    data.insert("hash".into(), scan.extr(">", "</span>").into());
    data
}

fn acidimg_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let (url, double) = match scan.find("<img class='centred' src='", "'") {
        Some(url) if !url.is_empty() => (url, false),
        _ => (
            found(scan.extr("<img class=\"centred\" src=\"", "\""))?,
            true,
        ),
    };
    let filename = match (double, scan.find("alt='", "'")) {
        (false, Some(name)) if !name.is_empty() => Some(name),
        _ => scan.find("alt=\"", "\""),
    };
    Ok((
        url.to_owned(),
        filename.filter(|f| !f.is_empty()).map(str::to_owned),
    ))
}

fn imagevenue_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let start = page.find("class=\"card-body").ok_or_else(not_found)?;
    let mut scan = Scan::at(page, start);
    let mut url = scan.extr("<img src=\"", "\"");
    if url.ends_with("/loader.svg") {
        url = scan.extr("<img src=\"", "\"");
    }
    let filename = unescape(scan.extr("alt=\"", "\""));
    Ok((url.to_owned(), Some(filename)))
}

fn imagetwist_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = scan.extr("<img src=\"", "\"");
    if url.starts_with("/imgs/") {
        return Err(not_found());
    }
    let filename = scan.find(" alt=\"", "\"").map(str::to_owned);
    Ok((url.to_owned(), filename))
}

fn imgadult_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = scan.extr("' src='", "'").to_owned();
    let name = scan
        .find("alt='", "'")
        .filter(|n| !n.is_empty())
        .map(|name| {
            let (left, _, right) = rpartition(name, " image hosted at ImgAdult.com");
            unescape(if left.is_empty() { right } else { left })
        });
    Ok((url, name))
}

fn imgspice_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let start = page.find("id=\"imgpreview\"").ok_or_else(not_found)?;
    let mut scan = Scan::at(page, start);
    let url = scan.extr("src=\"", "\"").to_owned();
    Ok((url, Some(unescape(scan.extr("alt=\"", "\"")))))
}

fn pixhost_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = scan.extr("class=\"image-img\" src=\"", "\"").to_owned();
    let name = scan
        .find("alt=\"", "\"")
        .filter(|n| !n.is_empty())
        .map(unescape);
    Ok((url, name))
}

fn turbo_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let start = page.find("<img ").ok_or_else(not_found)?;
    Ok((Scan::at(page, start).extr("src=\"", "\"").to_owned(), None))
}

fn vipr_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = scan.extr("<img src=\"", "\"");
    if !url.starts_with('h') {
        return Err(not_found());
    }
    let alt = scan
        .find(" alt=\"", "\"")
        .filter(|a| !a.is_empty())
        .map(unescape);
    Ok((url.to_owned(), alt))
}

fn imgclick_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = scan.extr("<br><img src=\"", "\"").to_owned();
    Ok((url, scan.find("alt=\"", "\"").map(str::to_owned)))
}

fn fappic_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = scan.extr("<a href=\"#\"><img src=\"", "\"").to_owned();
    let name = scan.extr("alt=\"", "\"");
    Ok((
        url,
        Some(
            re!(r"^Porn[ -]Pic(?:s|ture)[ -]")
                .replace(name, "")
                .into_owned(),
        ),
    ))
}

fn picstate_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let start = page.find(" id=\"image_container\"").ok_or_else(not_found)?;
    let mut scan = Scan::at(page, start);
    let url = scan.extr("<img src=\"", "\"").to_owned();
    Ok((url, scan.find("alt=\"", "\"").map(str::to_owned)))
}

fn imgdrive_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let title = scan.extr("property=\"og:title\" content=\"", "\"");
    let image = scan.extr("property=\"og:image\" content=\"", "\"");
    let mut parts: Vec<&str> = title.rsplitn(3, " | ").collect();
    parts.reverse();
    Ok((image.replace("/small/", "/big/"), Some(parts[0].to_owned())))
}

fn silverpic_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let path = scan.extr("<img src=\"/img/", "\"");
    let alt = scan.find("alt=\"", "\"").map(str::to_owned);
    Ok((format!("https://silverpic.net/img/{path}"), alt))
}

fn silverpic_meta(page: &str) -> Map<String, Value> {
    let start = page.find("<img src=\"/img/").unwrap_or(0);
    let width = Scan::at(page, start).extr("width=\"", "\"").to_owned();
    let height = Scan::at(page, start).extr("height=\"", "\"").to_owned();
    let mut data = Map::new();
    data.insert("width".into(), parse_int(&width).into());
    data.insert("height".into(), parse_int(&height).into());
    data
}

fn imgpv_info(page: &str) -> crate::Result<(String, Option<String>)> {
    let mut scan = Scan::new(page);
    let url = scan.extr("id=\"img-preview\" src=\"", "\"").to_owned();
    Ok((url, Some(unescape(scan.extr("alt=\"", "\"")))))
}

fn imgpv_meta(page: &str) -> Map<String, Value> {
    let start = page.find("class=\"upinfo\">").unwrap_or(0);
    let mut scan = Scan::at(page, start);
    let stamp = scan.extr("<b>", "by").to_owned();
    let user = scan.extr(">", "<");
    let parts: Vec<&str> = stamp.split_whitespace().collect();
    let iso = match parts.as_slice() {
        [day, time, ..] => format!("{} {time}", day.get(..10).unwrap_or(day)),
        _ => String::new(),
    };
    let mut data = Map::new();
    data.insert("date".into(), date(parse_iso(&iso)));
    data.insert("user".into(), unescape(user).into());
    data
}

/// Queue image URLs of this host with `data`.
fn queue<'a>(
    http: &'a Http,
    site: &'static Site,
    urls: Vec<String>,
    data: &Map<String, Value>,
) -> Events<'a> {
    let children = urls
        .into_iter()
        .filter_map(|u| Url::parse(&u).ok())
        .map(|u| (u, data.clone()))
        .collect::<Vec<_>>();
    use futures_util::StreamExt;
    futures_util::stream::iter(
        children
            .into_iter()
            .filter(|(u, _)| (site.matches)(u))
            .map(move |(u, d)| crate::sites::queue(http, site.category, "gallery", u, &d)),
    )
    .flatten()
    .boxed()
}

fn imxto_gallery<'a>(
    http: &'a Http,
    site: &'static Site,
    caps: regex::Captures<'_>,
    _: Url,
) -> Events<'a> {
    let page_url = format!("https://{}", group(&caps, 1));
    crate::sites::paginate(Some((1u64, None::<Map<String, Value>>)), move |state| {
        let page_url = page_url.clone();
        async move {
            let Some((number, data)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&page_url)?;
            if number > 1 {
                url.query_pairs_mut()
                    .append_pair("page", &number.to_string());
            }
            let page = http.text(&url).await?;
            let mut scan = Scan::new(&page);
            let title = scan.extr("<div class=\"title", "<");
            let pos = if number == 1 { scan.pos() } else { 0 };
            let data = data.unwrap_or_else(|| {
                let mut data = Map::new();
                data.insert(
                    "title".into(),
                    unescape(partition(title, ">").2).trim().into(),
                );
                data
            });
            let urls = extract_iter(&page[pos..], "<a href=", " ")
                .into_iter()
                .filter(|u| u.contains("/i/"))
                .map(|u| u.trim_matches(['"', '\'']).to_owned())
                .collect();
            let events = queue(http, site, urls, &data);
            let done =
                !page.contains("class=\"pagination") || page.contains("class=\"disabled\">Last");
            Ok(Some((events, (!done).then_some((number + 1, Some(data))))))
        }
    })
}

fn imagetwist_gallery<'a>(
    http: &'a Http,
    site: &'static Site,
    caps: regex::Captures<'_>,
    _: Url,
) -> Events<'a> {
    let page_url = format!("https://{}", group(&caps, 1));
    let root = page_url[..page_url[8..].find('/').map_or(page_url.len(), |i| i + 8)].to_owned();
    let gallery_id = caps.get(3).map(|m| m.as_str().to_owned());
    crate::sites::paginate(Some((page_url, None::<Map<String, Value>>)), move |state| {
        let root = root.clone();
        let gallery_id = gallery_id.clone();
        async move {
            let Some((url, data)) = state else {
                return Ok(None);
            };
            let page = http.text(&http.api(&url)?).await?;
            let data = data.unwrap_or_else(|| {
                let mut scan = Scan::new(&page);
                let mut data = Map::new();
                data.insert(
                    "gallery_title".into(),
                    unescape(scan.extr("page_main_title\">", "<")).into(),
                );
                let id = gallery_id.unwrap_or_else(|| scan.extr("&amp;fld_id=", "&").to_owned());
                data.insert("gallery_id".into(), id.into());
                data
            });
            let gallery = extr(&page, "class=\"gallerys", "</div");
            let urls = extract_iter(gallery, " href=\"", "\"")
                .into_iter()
                .map(|p| format!("{root}{p}"))
                .collect();
            let events = queue(http, site, urls, &data);
            let next = page.find("&#187;</a>").map(|pos| {
                let qs = unescape(rextr(&page, "href='", "'", Some(pos)));
                (format!("{root}/{qs}"), Some(data))
            });
            Ok(Some((events, next)))
        }
    })
}

fn pixhost_gallery<'a>(
    http: &'a Http,
    site: &'static Site,
    caps: regex::Captures<'_>,
    _: Url,
) -> Events<'a> {
    let page_url = format!("https://pixhost.to{}", group(&caps, 1));
    crate::sites::once(async move {
        let page = http.text(&http.api(&page_url)?).await?;
        let images = extr(&page, "class=\"images\"", "</div>");
        let urls = extract_iter(images, "<a href=\"", "\"")
            .into_iter()
            .map(str::to_owned)
            .collect();
        Ok(queue(http, site, urls, &Map::new()))
    })
}

fn turbo_gallery<'a>(
    http: &'a Http,
    site: &'static Site,
    caps: regex::Captures<'_>,
    _: Url,
) -> Events<'a> {
    let page_url = format!("https://{}", group(&caps, 1));
    crate::sites::paginate(Some(1u64), move |state| {
        let page_url = page_url.clone();
        async move {
            let Some(number) = state else { return Ok(None) };
            let mut url = http.api(&page_url)?;
            url.query_pairs_mut().append_pair("p", &number.to_string());
            let page = http.text(&url).await?;
            if number == 1 && page.contains("Requested gallery don`t exist on our website.") {
                return Err(error("TurboImageHost gallery not found"));
            }
            let urls: Vec<String> = extract_iter(&page, "\"><a href=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
            let next = (!urls.is_empty()).then_some(number + 1);
            Ok(Some((queue(http, site, urls, &Map::new()), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    fn matched(site: &Site, raw: &str) -> bool {
        (site.matches)(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        for (site, raw) in [
            (&IMXTO, "https://imx.to/i/1qdeva"),
            (&IMXTO, "https://img.yt/img-57a2050547b97.html"),
            (&IMXTO, "https://imx.to/g/ozdy"),
            (
                &IMAGETWIST,
                "https://phun.imagetwist.com/f1i2s4vhvbrq/test.png",
            ),
            (&IMAGETWIST, "https://imagehaha.com/f1i2s4vhvbrq/test.png"),
            (&IMAGETWIST, "https://imagetwist.com/p/gdldev/747223/digits"),
            (
                &IMAGETWIST,
                "https://imagetwist.com/?op=user_public&per_page=40&fld_id=806105&usr_login=gdldev&page=2",
            ),
            (&ACIDIMG, "https://acidimg.cc/img-5acb6b9de4640.html"),
            (&PIXHOST, "https://pixhost.to/show/190/130327671_test-.png"),
            (&PIXHOST, "https://pixhost.to/gallery/jSMFq"),
            (&VIPR, "https://vipr.im/yyqomiutt768"),
            (&FAPPIC, "https://www.fappic.com/i/00133/qi2nplzmwq7d_t.jpg"),
            (&IMAGEVENUE, "https://www.imagevenue.com/ME13LS07"),
            (
                &IMAGEVENUE,
                "http://img159.imagevenue.com/img.php?image=73874_203_123_83lo.jpg",
            ),
            (&IMGADULT, "https://imgadult.com/img-686b4f451af05.html"),
            (
                &IMGSPICE,
                "https://imgspice.com/q1taxkhxprrn/58410038_cal022jsp_308191001.jpg.html",
            ),
            (
                &IMGCLICK,
                "http://imgclick.net/4tbrre1oxew9/test-_-_.png.html",
            ),
            (&PICSTATE, "https://picstate.com/view/full/23416694_sfyue"),
            (&IMGDRIVE, "https://imgdrive.net/img-61ac0caeabf35.html"),
            (&IMGWALLET, "https://imgwallet.com/img-62fd3045ceec6.html"),
            (&IMGTAXI, "https://imgtaxi.com/img-61c71cea940d6.html"),
            (
                &SILVERPIC,
                "https://www.silverpic.com/8k562jyix8xq/jxU_0001.JPG.html",
            ),
            (&IMGPV, "https://imgpv.com/30zydqn6y1yk/4bi%20(1).jpg.html"),
            (
                &TURBOIMAGEHOST,
                "https://www.turboimagehost.com/p/39078423/test--.png.html",
            ),
            (
                &TURBOIMAGEHOST,
                "https://www.turboimagehost.com/album/344597/testimagegallery",
            ),
        ] {
            assert!(matched(site, raw), "{} {raw}", site.category);
        }
        assert!(!matched(
            &IMGDRIVE,
            "https://imgtaxi.com/img-61c71cea940d6.html"
        ));
        assert!(!matched(&VIPR, "https://example.com/x"));
    }

    #[tokio::test]
    async fn imxto_posts_continue_form_and_reads_metadata() {
        let page = r#"<div style="text-align:center;"><a href="http://i001.imx.to/i/2021/a.jpg"><img title="test.jpg">
            [ FILESIZE <span>1.5 KB</span> | <span>800x600 px</span> | <span>abcd</span>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("imxto", root);
        let files = media((IMXTO.extract)(
            &http,
            Url::parse("https://img.yt/img-57a2050547b97.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://i001.imx.to/i/2021/a.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "test");
        assert_eq!(k["token"], "57a2050547b97");
        assert_eq!(k["post_url"], "https://imx.to/img-57a2050547b97.html");
        assert_eq!(k["size"], 1536);
        assert_eq!(k["width"], 800);
        assert_eq!(k["hash"], "abcd");
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{token}",["57a2050547b97"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /img-57a2050547b97.html "));
        assert!(requests[0].ends_with("imgContinue=Continue%2Bto%2Bimage%2B...%2B"));
    }

    #[tokio::test]
    async fn imgdrive_family_and_fappic() {
        let page = r#"<meta property="og:title" content="photo.jpg | ImgDrive | Free"><meta property="og:image" content="https://imgdrive.net/images/small/ab/photo.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("imgtaxi", root);
        let files = media((IMGTAXI.extract)(
            &http,
            Url::parse("https://imgtaxi.com/img-61c71cea940d6.html").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://imgdrive.net/images/big/ab/photo.jpg"
        );
        assert_eq!(files[0].category, "imgtaxi");
        assert_eq!(keywords(&files[0])["filename"], "photo");
        server.join().unwrap();

        let page = r##"<a href="#"><img src="https://img.fappic.com/x/qi2nplzmwq7d.jpg" alt="Porn Pics test.jpg">"##;
        let (root, server) = serve(vec![html(page)]);
        let http = client("fappic", root);
        let files = media((FAPPIC.extract)(
            &http,
            Url::parse("https://www.fappic.com/i/00133/qi2nplzmwq7d_t.jpg").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["token"], "qi2nplzmwq7d");
        assert_eq!(keywords(&files[0])["filename"], "test");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /qi2nplzmwq7d/pic.jpg "));
    }

    #[tokio::test]
    async fn imagevenue_reads_cookies_first() {
        let page = r#"<div class="card-body"><img src="https://cdn.imagevenue.com/loader.svg"><img src="https://cdno.imagevenue.com/a.jpg" alt="A &amp; B.jpg">"#;
        let (root, server) = serve(vec![html("cookies"), html(page)]);
        let http = client("imagevenue", root);
        let files = media((IMAGEVENUE.extract)(
            &http,
            Url::parse("https://www.imagevenue.com/ME13LS07").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://cdno.imagevenue.com/a.jpg");
        assert_eq!(keywords(&files[0])["filename"], "A & B");
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn galleries_queue_images() {
        let page = r#"<div class="images"><a href="https://pixhost.to/show/1/2_a.jpg"></a><a href="https://pixhost.to/show/1/3_b.jpg"></a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("pixhost", root);
        assert_eq!(
            children((PIXHOST.extract)(
                &http,
                Url::parse("https://pixhost.to/gallery/jSMFq").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();

        let first = r#"<a href="/x">"><a href="https://www.turboimagehost.com/p/1/a.jpg.html">"#;
        let (root, server) = serve(vec![html(first), html("<html></html>")]);
        let http = client("turboimagehost", root);
        assert_eq!(
            children((TURBOIMAGEHOST.extract)(
                &http,
                Url::parse("https://www.turboimagehost.com/album/1/x").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /album/1/x?p=2 "));

        let first = r#"<div class="title x">Title</div><a href="https://imx.to/i/abc" class=x><div class="pagination"><a>Last</a>"#;
        let last = r#"<a href='https://imx.to/i/def' ><div class="pagination"><span class="disabled">Last"#;
        let (root, server) = serve(vec![html(first), html(last)]);
        let http = client("imxto", root);
        assert_eq!(
            children((IMXTO.extract)(
                &http,
                Url::parse("https://imx.to/g/ozdy").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /g/ozdy?page=2 "));
    }
}
