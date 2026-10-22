//! SteamGridDB assets and game/collection asset lists, mirroring
//! gallery_dl/extractor/steamgriddb.py through the public `/api/public`
//! endpoints, with upstream's fixed `userprefs` cookie.
use crate::sites::art::{nameext_into, truthy};
use crate::sites::{Events, Http, Item, Route, Site, scalar};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "steamgriddb",
    name: "SteamGridDB",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ASSET, &GRIDS, &HEROES, &LOGOS, &ICONS],
    api_key: false,
};

const ROOT: &str = "https://www.steamgriddb.com";
const COOKIE: &str = "userprefs=%7B%22adult%22%3Afalse%7D";
const FILENAME: &str = "{game[id]}_{id}_{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{subcategory}", "{game[id]}"];
static ASSET: Route = Route::new("asset", FILENAME, DIRECTORY, "{filename}");
static GRIDS: Route = Route::new("grids", FILENAME, DIRECTORY, "{filename}");
static HEROES: Route = Route::new("heroes", FILENAME, DIRECTORY, "{filename}");
static LOGOS: Route = Route::new("logos", FILENAME, DIRECTORY, "{filename}");
static ICONS: Route = Route::new("icons", FILENAME, DIRECTORY, "{filename}");

const LANGUAGE_CODES: &[&str] = &[
    "aa", "ab", "ae", "af", "ak", "am", "an", "ar", "as", "av", "ay", "az", "ba", "be", "bg", "bh",
    "bi", "bm", "bn", "bo", "br", "bs", "ca", "ce", "ch", "co", "cr", "cs", "cu", "cv", "cy", "da",
    "de", "dv", "dz", "ee", "el", "en", "eo", "es", "et", "eu", "fa", "ff", "fi", "fj", "fo", "fr",
    "fy", "ga", "gd", "gl", "gn", "gu", "gv", "ha", "he", "hi", "ho", "hr", "ht", "hu", "hy", "hz",
    "ia", "id", "ie", "ig", "ii", "ik", "io", "is", "it", "iu", "ja", "jv", "ka", "kg", "ki", "kj",
    "kk", "kl", "km", "kn", "ko", "kr", "ks", "ku", "kv", "kw", "ky", "la", "lb", "lg", "li", "ln",
    "lo", "lt", "lu", "lv", "mg", "mh", "mi", "mk", "ml", "mn", "mr", "ms", "mt", "my", "na", "nb",
    "nd", "ne", "ng", "nl", "nn", "no", "nr", "nv", "ny", "oc", "oj", "om", "or", "os", "pa", "pi",
    "pl", "ps", "pt", "qu", "rm", "rn", "ro", "ru", "rw", "sa", "sc", "sd", "se", "sg", "si", "sk",
    "sl", "sm", "sn", "so", "sq", "sr", "ss", "st", "su", "sv", "sw", "ta", "te", "tg", "th", "ti",
    "tk", "tl", "tn", "to", "tr", "ts", "tt", "tw", "ty", "ug", "uk", "ur", "uz", "ve", "vi", "vo",
    "wa", "wo", "xh", "yi", "yo", "za", "zh", "zu",
];
const ICON_SIZES: &[u32] = &[
    8, 10, 14, 16, 20, 24, 28, 32, 35, 40, 48, 54, 56, 57, 60, 64, 72, 76, 80, 90, 96, 100, 114,
    120, 128, 144, 150, 152, 160, 180, 192, 194, 256, 310, 512, 768, 1024,
];

/// One asset-list class: its API type and accepted filter values.
struct Kind {
    route: &'static Route,
    asset_type: &'static str,
    dimensions: Option<&'static [&'static str]>,
    styles: &'static [&'static str],
    file_types: &'static [&'static str],
}

const KINDS: &[(&str, Kind)] = &[
    (
        "grids",
        Kind {
            route: &GRIDS,
            asset_type: "grid",
            dimensions: Some(&[
                "460x215",
                "920x430",
                "600x900",
                "342x482",
                "660x930",
                "512x512",
                "1024x1024",
            ]),
            styles: &["alternate", "blurred", "no_logo", "material", "white_logo"],
            file_types: &["png", "jpeg", "jpg", "webp"],
        },
    ),
    (
        "heroes",
        Kind {
            route: &HEROES,
            asset_type: "hero",
            dimensions: Some(&["1920x620", "3840x1240", "1600x650"]),
            styles: &["alternate", "blurred", "material"],
            file_types: &["png", "jpeg", "jpg", "webp"],
        },
    ),
    (
        "logos",
        Kind {
            route: &LOGOS,
            asset_type: "logo",
            dimensions: None,
            styles: &["official", "white", "black", "custom"],
            file_types: &["png", "webp"],
        },
    ),
    (
        "icons",
        Kind {
            route: &ICONS,
            asset_type: "icon",
            // Checked against ICON_SIZES as `NxN`.
            dimensions: Some(&[]),
            styles: &["official", "custom"],
            file_types: &["png", "ico"],
        },
    ),
];

#[derive(Debug, PartialEq)]
enum Target {
    Asset {
        asset_type: String,
        id: String,
    },
    List {
        kind: &'static str,
        collection: bool,
        id: u64,
        page: i64,
    },
}

/// Leading ASCII digits of a segment, as an unanchored `(\d+)`.
fn leading_digits(text: &str) -> Option<&str> {
    let end = text.bytes().take_while(u8::is_ascii_digit).count();
    (1..=18).contains(&end).then(|| &text[..end])
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["steamgriddb.com"]) {
        return None;
    }
    let path = url.path();
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    match parts.as_slice() {
        [kind @ ("grid" | "hero" | "logo" | "icon"), id, ..] => Some(Target::Asset {
            asset_type: (*kind).into(),
            id: leading_digits(id)?.into(),
        }),
        [list @ ("game" | "collection"), id, rest @ ..] if leading_digits(id) == Some(id) => {
            let (kind, page) = match rest {
                [] => return None,
                [kind, tail @ ..] => {
                    let (name, _) = KINDS.iter().find(|(name, _)| kind.starts_with(name))?;
                    // `/KIND(?:/(\d+))?` without an end anchor.
                    let page = match (kind.len() == name.len(), tail) {
                        (true, [page, ..]) => leading_digits(page).and_then(|p| p.parse().ok()),
                        _ => None,
                    };
                    (*name, page.unwrap_or(1))
                }
            };
            Some(Target::List {
                kind,
                collection: *list == "collection",
                id: id.parse().ok()?,
                page,
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Asset { asset_type, id }) => crate::sites::once(async move {
            let url = http.api(&format!("{ROOT}/api/public/asset/{asset_type}/{id}"))?;
            let data = call(http.json_with(&url, &[("Cookie", COOKIE)]).await?)?;
            let asset = data
                .get("asset")
                .filter(|a| !a.is_null())
                .ok_or_else(|| error(format!("SteamGridDB asset ({asset_type}:{id}) not found")))?;
            Ok(crate::sites::events(asset_events(http, &ASSET, asset)))
        }),
        Some(Target::List {
            kind,
            collection,
            id,
            page,
        }) => {
            let kind = &KINDS.iter().find(|(name, _)| *name == kind).unwrap().1;
            let body = match search_body(http, kind, collection, id) {
                Ok(body) => body,
                Err(e) => return crate::sites::fail(e),
            };
            assets(http, kind.route, body, page)
        }
        None => crate::sites::fail(error("unsupported SteamGridDB URL")),
    }
}

/// Upstream `_call`: unwrap `data`, aborting on `success: false`.
fn call(body: Value) -> Result<Value> {
    if !truthy(body.get("success")) {
        let message: String = body
            .get("error")
            .map(scalar)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(error(format!("SteamGridDB API error: {message}")));
    }
    Ok(body.get("data").cloned().unwrap_or(Value::Null))
}

/// Upstream `config_list`: `["all"]` unless a valid value list is set.
fn config_list(
    http: &Http,
    key: &str,
    type_name: &str,
    valid: &dyn Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let values: Vec<String> = match http.option(key) {
        None | Some(Value::Null) => return Ok(vec!["all".into()]),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(items)) => items.iter().map(scalar).collect(),
        Some(other) => vec![scalar(other)],
    };
    if values.iter().any(|v| v == "all") {
        return Ok(vec!["all".into()]);
    }
    for value in &values {
        if !valid(value) {
            let shown: String = value.chars().take(64).collect();
            return Err(error(format!("Invalid {type_name} '{shown}'")));
        }
    }
    Ok(values)
}

fn mime(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpeg" | "jpg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/vnd.microsoft.icon",
        _ => "all",
    }
}

/// The upstream search request body for one asset list.
fn search_body(http: &Http, kind: &Kind, collection: bool, id: u64) -> Result<Map<String, Value>> {
    let sort = http.option_str("sort").unwrap_or("score_desc");
    if !matches!(
        sort,
        "score_desc" | "score_asc" | "score_old_desc" | "score_old_asc" | "age_desc" | "age_asc"
    ) {
        let shown: String = sort.chars().take(64).collect();
        return Err(error(format!("Invalid sort '{shown}'")));
    }
    let mut body = Map::new();
    for key in [
        "static", "animated", "humor", "nsfw", "epilepsy", "untagged",
    ] {
        let value = match http.option(key) {
            None => Value::Bool(true),
            Some(value) => value.clone(),
        };
        body.insert(key.into(), value);
    }
    body.insert("asset_type".into(), kind.asset_type.into());
    body.insert("limit".into(), LIMIT.into());
    body.insert("order".into(), sort.into());
    if let Some(dimensions) = kind.dimensions {
        let icon = kind.asset_type == "icon";
        let valid = |v: &str| {
            if icon {
                ICON_SIZES.iter().any(|n| v == format!("{n}x{n}"))
            } else {
                dimensions.contains(&v)
            }
        };
        body.insert(
            "dimensions".into(),
            json!(config_list(http, "dimensions", "dimension", &valid)?),
        );
    }
    let styles = kind.styles;
    body.insert(
        "styles".into(),
        json!(config_list(http, "styles", "style", &|v| styles.contains(&v))?),
    );
    body.insert(
        "languages".into(),
        json!(config_list(http, "languages", "language", &|v| {
            LANGUAGE_CODES.contains(&v)
        })?),
    );
    let file_types = kind.file_types;
    let types = config_list(http, "file-types", "file type", &|v| {
        file_types.contains(&v)
    })?;
    body.insert(
        "mime".into(),
        json!(types.iter().map(|t| mime(t)).collect::<Vec<_>>()),
    );
    if collection {
        body.insert("collection_id".into(), id.into());
    } else {
        body.insert("game_id".into(), json!([id]));
    }
    Ok(body)
}

const LIMIT: i64 = 48;

fn assets<'a>(
    http: &'a Http,
    route: &'static Route,
    body: Map<String, Value>,
    start: i64,
) -> Events<'a> {
    // Upstream computes `min(page - 1, 0)`, so listings always start at 0.
    let page = (start - 1).min(0);
    crate::sites::paginate(Some(page), move |page| {
        let mut body = body.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            body.insert("page".into(), page.into());
            let url = http.api(&format!("{ROOT}/api/public/search/assets"))?;
            let data = call(
                http.post_json(&url, &Value::Object(body), &[("Cookie", COOKIE)])
                    .await?,
            )?;
            let game = data.get("game").cloned().unwrap_or(Value::Null);
            let mut events = Vec::new();
            for asset in data
                .get("assets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let mut asset = asset.clone();
                if !truthy(asset.get("game"))
                    && let Some(fields) = asset.as_object_mut()
                {
                    fields.insert("game".into(), game.clone());
                }
                events.extend(asset_events(http, route, &asset));
            }
            let total = data.get("total").and_then(Value::as_i64).unwrap_or(0);
            let next = (total > LIMIT * page && page < 100_000).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn asset_events(
    http: &Http,
    route: &'static Route,
    asset: &Value,
) -> Vec<crate::event::ExtractionEvent> {
    let fake_png = http.option_bool("download-fake-png", true);
    crate::sites::post(&SITE, http, route, parse_asset(asset, fake_png))
}

/// One asset's file and, by default, its `fake_png` companion.
fn parse_asset(asset: &Value, download_fake_png: bool) -> Vec<Item> {
    let Some(fields) = asset.as_object() else {
        return Vec::new();
    };
    let fake = fields
        .get("fake_png")
        .filter(|v| download_fake_png && truthy(Some(v)))
        .and_then(Value::as_str);
    let mut data = fields.clone();
    data.insert("count".into(), if fake.is_some() { 2 } else { 1 }.into());
    let id = data.get("id").map(scalar).unwrap_or_default();
    let user = data
        .get("author")
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut items = Vec::new();
    let urls = [fields.get("url").and_then(Value::as_str), fake];
    for (index, raw) in urls.into_iter().enumerate() {
        let Some(raw) = raw else {
            continue;
        };
        let Some(url) = crate::sites::art::file_url(raw, ROOT) else {
            continue;
        };
        let mut file = data.clone();
        file.insert("num".into(), (index + 1).into());
        nameext_into(raw, &mut file);
        let name = file["filename"].as_str().unwrap_or_default().to_owned();
        items.push(
            Item::new(url, file)
                .ids(id.clone(), name)
                .user(user.clone()),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("https://www.steamgriddb.com/grid/132605"),
            Some(Target::Asset {
                asset_type: "grid".into(),
                id: "132605".into()
            })
        );
        assert_eq!(
            t("https://steamgriddb.com/game/5259324/grids/3"),
            Some(Target::List {
                kind: "grids",
                collection: false,
                id: 5259324,
                page: 3
            })
        );
        assert_eq!(
            t("https://www.steamgriddb.com/collection/332/heroes"),
            Some(Target::List {
                kind: "heroes",
                collection: true,
                id: 332,
                page: 1
            })
        );
        for raw in [
            "https://www.steamgriddb.com/grid/",
            "https://www.steamgriddb.com/game/12/",
            "https://www.steamgriddb.com/game/12x/grids",
            "https://www.steamgriddb.com/game/12/videos",
            "https://x.steamgriddb.com/grid/1",
            "https://www.steamgriddb.com.evil.com/grid/1",
            "https://evilsteamgriddb.com/grid/1",
            "https://www.steamgriddb.com:8443/grid/1",
            "https://u:p@www.steamgriddb.com/grid/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn asset(id: u64, fake: bool) -> Value {
        json!({"id": id, "score": 1, "style": "alternate", "width": 600, "height": 900,
            "url": format!("https://cdn2.steamgriddb.com/grid/{id}abc.png"),
            "thumb": "https://cdn2.steamgriddb.com/thumb/x.jpg",
            "fake_png": if fake { json!(format!("https://cdn2.steamgriddb.com/grid/{id}fake.png")) } else { json!(null) },
            "author": {"name": "someone"}, "game": {"id": 5247997, "name": "OMORI"}})
    }

    #[tokio::test]
    async fn extracts_asset_with_fake_png() {
        let (root, server) = serve(vec![json(
            json!({"success": true, "data": {"asset": asset(132605, true)}}),
        )]);
        let http = http("steamgriddb", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.steamgriddb.com/grid/132605").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "132605fake");
        assert_eq!(k["game"]["name"], "OMORI");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "5247997_132605_01.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("steamgriddb/asset/5247997")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{filename}",["132605abc"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/public/asset/grid/132605 "));
        assert!(requests[0].contains("userprefs=%7B%22adult%22%3Afalse%7D"));
    }

    #[tokio::test]
    async fn reports_missing_and_failed_assets() {
        assert!(call(json!({"success": false, "error": "nope"})).is_err());
        let (root, _server) = serve(vec![json(
            json!({"success": true, "data": {"asset": null}}),
        )]);
        let http = http("steamgriddb", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://www.steamgriddb.com/hero/1").unwrap(),
        ))
        .await;
        assert!(events.iter().any(Result::is_err));
    }

    #[tokio::test]
    async fn paginates_search_until_total() {
        let page = |ids: &[u64], total| {
            json(json!({"success": true, "data": {
                "assets": ids.iter().map(|id| {
                    let mut a = asset(*id, false);
                    a["game"] = Value::Null;
                    a
                }).collect::<Vec<_>>(),
                "game": {"id": 7, "name": "G"}, "total": total}}))
        };
        let (root, server) = serve(vec![page(&[1], 50), page(&[2], 50), page(&[], 50)]);
        let http = http("steamgriddb", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.steamgriddb.com/game/7/grids/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["game"]["id"], 7);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("steamgriddb/grids/7")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("POST /api/public/search/assets "));
        let body: Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["page"], 0);
        assert_eq!(body["game_id"], json!([7]));
        assert_eq!(body["mime"], json!(["all"]));
        assert_eq!(body["dimensions"], json!(["all"]));
        assert_eq!(body["order"], "score_desc");
        assert_eq!(body["nsfw"], true);
        let body: Value =
            serde_json::from_str(requests[2].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["page"], 2);
    }

    #[test]
    fn options_shape_the_search() {
        let options: Map<String, Value> = serde_json::from_value(json!({
            "sort": "age_asc", "file-types": "png,ico", "dimensions": ["16x16"],
            "humor": false, "download-fake-png": false,
        }))
        .unwrap();
        let (root, _server) = serve(vec![]);
        let http = http("steamgriddb", root).with_options(Some(std::sync::Arc::new(options)));
        let icons = &KINDS[3].1;
        let body = search_body(&http, icons, true, 3).unwrap();
        assert_eq!(body["order"], "age_asc");
        assert_eq!(
            body["mime"],
            json!(["image/png", "image/vnd.microsoft.icon"])
        );
        assert_eq!(body["dimensions"], json!(["16x16"]));
        assert_eq!(body["humor"], false);
        assert_eq!(body["collection_id"], 3);
        assert!(!body.contains_key("game_id"));
        assert!(search_body(&http, &KINDS[0].1, false, 1).is_err());
        assert!(!KINDS[2].1.dimensions.is_some());
        assert_eq!(parse_asset(&asset(1, true), false).len(), 1);
    }
}
