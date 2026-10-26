//! Shopify store collections and products, mirroring
//! gallery_dl/extractor/shopify.py, with `shopify:https://...` for stores
//! without a built-in instance.
use crate::error;
use crate::sites::gdl::nameext_into;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{product[title]}_{num:>02}_{id}.{extension}";

static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &["{category}", "{collection[title]}"],
    "{id}",
);
static PRODUCT: Route = Route::new("product", FILENAME, &["{category}", "Products"], "{id}");

struct Instance {
    category: &'static str,
    root: &'static str,
    /// Host pattern without the scheme.
    pattern: &'static str,
}

macro_rules! instance {
    ($site:ident, $inst:ident, $category:literal, $name:literal, $root:literal, $pattern:literal) => {
        static $inst: Instance = Instance {
            category: $category,
            root: $root,
            pattern: $pattern,
        };
        pub static $site: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$inst, url).is_some(),
            extract: |http, url| extract(&$inst, &$site, http, url),
            routes: &[&COLLECTION, &PRODUCT],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(
    CHELSEACREW,
    CHELSEACREW_I,
    "chelseacrew",
    "Chelsea Crew",
    "https://chelseacrew.com",
    r"(?:www\.)?chelseacrew\.com"
);
instance!(
    FASHIONNOVA,
    FASHIONNOVA_I,
    "fashionnova",
    "Fashion Nova",
    "https://www.fashionnova.com",
    r"(?:www\.)?fashionnova\.com"
);
instance!(
    LOUNGEUNDERWEAR,
    LOUNGEUNDERWEAR_I,
    "loungeunderwear",
    "Lounge Underwear",
    "https://loungeunderwear.com",
    r"(?:[a-z]+\.)?loungeunderwear\.com"
);
instance!(
    MICHAELSCAMERAS,
    MICHAELSCAMERAS_I,
    "michaelscameras",
    "Michaels Camera",
    "https://michaels.com.au",
    r"michaels\.com\.au"
);
instance!(
    MODCLOTH,
    MODCLOTH_I,
    "modcloth",
    "ModCloth",
    "https://modcloth.com",
    r"modcloth\.com"
);
instance!(
    OHPOLLY,
    OHPOLLY_I,
    "ohpolly",
    "Oh Polly",
    "https://www.ohpolly.com",
    r"(?:www\.)?ohpolly\.com"
);
instance!(
    OMGMIAMISWIMWEAR,
    OMGMIAMISWIMWEAR_I,
    "omgmiamiswimwear",
    "OMG Miami Swimwear",
    "https://www.omgmiamiswimwear.com",
    r"(?:www\.)?omgmiamiswimwear\.com"
);
instance!(
    PINUPGIRLCLOTHING,
    PINUPGIRLCLOTHING_I,
    "pinupgirlclothing",
    "Pinup Girl Clothing",
    "https://pinupgirlclothing.com",
    r"pinupgirlclothing\.com"
);
instance!(
    RAIDLONDON,
    RAIDLONDON_I,
    "raidlondon",
    "Raid London",
    "https://www.raidlondon.com",
    r"(?:www\.)?raidlondon\.com"
);
instance!(
    UNIQUEVINTAGE,
    UNIQUEVINTAGE_I,
    "unique-vintage",
    "Unique Vintage",
    "https://www.unique-vintage.com",
    r"(?:www\.)?unique\-vintage\.com"
);
instance!(
    WINDSORSTORE,
    WINDSORSTORE_I,
    "windsorstore",
    "Windsor",
    "https://www.windsorstore.com",
    r"(?:www\.)?windsorstore\.com"
);
// `shopify:https://store/...` for stores without a built-in instance.
instance!(SHOPIFY, SHOPIFY_I, "shopify", "Shopify", "", "");

#[derive(Debug, PartialEq)]
enum Kind {
    Collection(String),
    Product(String),
}

fn compiled(pattern: &'static str) -> regex::Regex {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<&'static str, regex::Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(pattern)
        .or_insert_with(|| {
            regex::Regex::new(&format!(r"^https?://{pattern}")).expect("static shopify pattern")
        })
        .clone()
}

/// Root and target; the locale prefix (`/en-us`) is skipped like upstream.
fn target(inst: &Instance, url: &Url) -> Option<(String, Kind)> {
    let raw = url.as_str();
    let (root, rest) = if inst.category == "shopify" {
        let c = crate::sites::gdl::re!(r"^shopify:(https?://[^/?#]+)(.*)$").captures(raw)?;
        (c[1].to_owned(), c[2].to_owned())
    } else {
        if !crate::sites::plain(url) {
            return None;
        }
        let c = compiled(inst.pattern).captures(raw)?;
        (inst.root.to_owned(), raw[c.get(0)?.end()..].to_owned())
    };
    const LOCALE: &str = r"^(?:/[a-z]{2}(?:-[^/?#]+)?)?";
    if let Some(c) = crate::sites::gdl::re!(&format!(r"{LOCALE}(/collections/[\w-]+)/?(?:$|[?#])"))
        .captures(&rest)
    {
        return Some((root, Kind::Collection(c[1].to_owned())));
    }
    let c = crate::sites::gdl::re!(&format!(
        r"{LOCALE}((?:/collections/[\w-]+)?/products/[\w-]+)"
    ))
    .captures(&rest)?;
    Some((root, Kind::Product(c[1].to_owned())))
}

/// `ShopifyExtractor.items` for one product.
fn product_items(mut product: Map<String, Value>, data: &Map<String, Value>) -> Vec<Item> {
    let images = product
        .remove("images")
        .and_then(|i| i.as_array().cloned())
        .unwrap_or_default();
    let product = Value::Object(product);
    images
        .iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let mut image = image.as_object()?.clone();
            let src = image.get("src").and_then(Value::as_str)?.to_owned();
            nameext_into(&src, &mut image);
            image.extend(data.clone());
            image.insert("product".into(), product.clone());
            image.insert("num".into(), (index + 1).into());
            let id = image
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            Some(
                Item::new(Url::parse(&src).ok()?, image)
                    .ids(crate::sites::scalar(&product["id"]), id),
            )
        })
        .collect()
}

fn extract<'a>(
    inst: &'static Instance,
    site: &'static Site,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some((root, kind)) = target(inst, &url) else {
        return crate::sites::fail(error("unsupported Shopify URL"));
    };
    match kind {
        Kind::Product(path) => crate::sites::once(async move {
            let data = http.json(&http.api(&format!("{root}{path}.json"))?).await?;
            let mut product = data["product"]
                .as_object()
                .cloned()
                .ok_or_else(|| error("Shopify returned no product"))?;
            product.remove("image");
            Ok(crate::sites::events(crate::sites::post(
                site,
                http,
                &PRODUCT,
                product_items(product, &Map::new()),
            )))
        }),
        Kind::Collection(path) => {
            crate::sites::paginate(Some((1u64, None::<Map<String, Value>>)), move |state| {
                let root = root.clone();
                let path = path.clone();
                async move {
                    let Some((page, data)) = state else {
                        return Ok(None);
                    };
                    let data = match data {
                        Some(data) => data,
                        None => http
                            .json(&http.api(&format!("{root}{path}.json"))?)
                            .await?
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    };
                    let mut url = http.api(&format!("{root}{path}/products.json"))?;
                    url.query_pairs_mut().append_pair("page", &page.to_string());
                    let products = http.json(&url).await?["products"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    if products.is_empty() {
                        return Ok(None);
                    }
                    let events = products
                        .into_iter()
                        .filter_map(|p| p.as_object().cloned())
                        .flat_map(|product| {
                            crate::sites::post(
                                site,
                                http,
                                &COLLECTION,
                                product_items(product, &data),
                            )
                        })
                        .collect();
                    Ok(Some((
                        crate::sites::events(events),
                        Some((page + 1, Some(data))),
                    )))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |inst: &Instance, raw: &str| target(inst, &Url::parse(raw).unwrap());
        assert_eq!(
            t(
                &FASHIONNOVA_I,
                "https://www.fashionnova.com/collections/mini-dresses/?page=1"
            ),
            Some((
                "https://www.fashionnova.com".into(),
                Kind::Collection("/collections/mini-dresses".into())
            ))
        );
        assert_eq!(
            t(
                &FASHIONNOVA_I,
                "https://www.fashionnova.com/en-us/collections/a/products/b"
            ),
            Some((
                "https://www.fashionnova.com".into(),
                Kind::Product("/collections/a/products/b".into())
            ))
        );
        assert_eq!(
            t(
                &LOUNGEUNDERWEAR_I,
                "https://de.loungeunderwear.com/products/x"
            ),
            Some((
                "https://loungeunderwear.com".into(),
                Kind::Product("/products/x".into())
            ))
        );
        assert_eq!(
            t(&SHOPIFY_I, "shopify:https://shop.example.com/products/x"),
            Some((
                "https://shop.example.com".into(),
                Kind::Product("/products/x".into())
            ))
        );
        assert!(t(&MODCLOTH_I, "https://www.modcloth.com/products/x").is_none());
    }

    #[tokio::test]
    async fn collection_pages_products() {
        let product = json!({"id": 7, "title": "Dress", "images": [{"id": 1, "src": "https://cdn.shopify.com/a.jpg?v=1"}, {"id": 2, "src": "https://cdn.shopify.com/b.png"}]});
        let (root, server) = serve(vec![
            json(json!({"collection": {"title": "Minis"}})),
            json(json!({"products": [product]})),
            json(json!({"products": []})),
        ]);
        let http = client("fashionnova", root);
        let files = media(extract(
            &FASHIONNOVA_I,
            &FASHIONNOVA,
            &http,
            Url::parse("https://www.fashionnova.com/collections/minis").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["product"]["title"], "Dress");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "Dress_02_2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("fashionnova/Minis")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /collections/minis/products.json?page=1 "));
    }
}
