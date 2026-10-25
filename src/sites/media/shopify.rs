//! Shopify stores, mirroring gallery_dl/extractor/shopify.py: the stores'
//! `.json` endpoints for a collection (paged `products.json`) or a single
//! product, with every product image.
//!
//! Configured `extractor.shopify.<category>` instances and `shopify:URL`
//! inputs go through `crate::sites::instances`.
use crate::error;
use crate::sites::art::nameext_into;
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

/// Which hosts an instance's pattern accepts.
#[derive(Clone, Copy)]
enum Hosts {
    /// The domain, with or without `www.`.
    Www(&'static str),
    /// Exactly the domain.
    Bare(&'static str),
    /// The domain or one lowercase-letter subdomain of it.
    Sub(&'static str),
    /// A configured instance's root without scheme, `host[:port][/path]`.
    Configured(&'static str),
}

/// One store: its root and accepted hosts.
struct Store {
    site: &'static Site,
    root: &'static str,
    hosts: Hosts,
}

macro_rules! instance {
    ($ident:ident, $store:ident, $category:literal, $name:literal, $root:literal, $hosts:expr) => {
        static $store: Store = Store {
            site: &$ident,
            root: $root,
            hosts: $hosts,
        };
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$hosts, url).is_some(),
            extract: |http, url| extract(&$ident, $root, &$hosts, http, url),
            routes: &[&COLLECTION, &PRODUCT],
            api_key: false,
            cookies: None,
        };
    };
}

/// Configured `extractor.shopify.<category>` instances.
pub(crate) static FAMILY: crate::sites::instances::Family = crate::sites::instances::Family {
    basecategory: "shopify",
    builtin: &[
        &CHELSEACREW,
        &FASHIONNOVA,
        &LOUNGEUNDERWEAR,
        &MICHAELSCAMERAS,
        &MODCLOTH,
        &OHPOLLY,
        &OMGMIAMISWIMWEAR,
        &PINUPGIRLCLOTHING,
        &RAIDLONDON,
        &UNIQUE_VINTAGE,
        &WINDSORSTORE,
    ],
    related: &[],
    template: &CHELSEACREW,
    matches: |category, url| target(&store(category).hosts, url).is_some(),
    extract: |category, http, url| {
        let store = store(category);
        extract(store.site, store.root, &store.hosts, http, url)
    },
};

static STORES: &[&Store] = &[
    &CHELSEACREW_STORE,
    &FASHIONNOVA_STORE,
    &LOUNGEUNDERWEAR_STORE,
    &MICHAELSCAMERAS_STORE,
    &MODCLOTH_STORE,
    &OHPOLLY_STORE,
    &OMGMIAMISWIMWEAR_STORE,
    &PINUPGIRLCLOTHING_STORE,
    &RAIDLONDON_STORE,
    &UNIQUE_VINTAGE_STORE,
    &WINDSORSTORE_STORE,
];

/// The store of a built-in or configured instance.
fn store(category: &str) -> Store {
    if let Some(configured) = crate::sites::instances::configured(&FAMILY, category) {
        return Store {
            site: &configured.site,
            root: configured.root,
            hosts: Hosts::Configured(configured.spec),
        };
    }
    let store = STORES
        .iter()
        .find(|store| store.site.category == category)
        .unwrap_or(&STORES[0]);
    Store {
        site: store.site,
        root: store.root,
        hosts: store.hosts,
    }
}
instance!(
    CHELSEACREW,
    CHELSEACREW_STORE,
    "chelseacrew",
    "Chelsea Crew",
    "https://chelseacrew.com",
    Hosts::Www("chelseacrew.com")
);
instance!(
    FASHIONNOVA,
    FASHIONNOVA_STORE,
    "fashionnova",
    "Fashion Nova",
    "https://www.fashionnova.com",
    Hosts::Www("fashionnova.com")
);
instance!(
    LOUNGEUNDERWEAR,
    LOUNGEUNDERWEAR_STORE,
    "loungeunderwear",
    "Lounge Underwear",
    "https://loungeunderwear.com",
    Hosts::Sub("loungeunderwear.com")
);
instance!(
    MICHAELSCAMERAS,
    MICHAELSCAMERAS_STORE,
    "michaelscameras",
    "Michaels Camera",
    "https://michaels.com.au",
    Hosts::Bare("michaels.com.au")
);
instance!(
    MODCLOTH,
    MODCLOTH_STORE,
    "modcloth",
    "ModCloth",
    "https://modcloth.com",
    Hosts::Bare("modcloth.com")
);
instance!(
    OHPOLLY,
    OHPOLLY_STORE,
    "ohpolly",
    "Oh Polly",
    "https://www.ohpolly.com",
    Hosts::Www("ohpolly.com")
);
instance!(
    OMGMIAMISWIMWEAR,
    OMGMIAMISWIMWEAR_STORE,
    "omgmiamiswimwear",
    "OMG Miami",
    "https://www.omgmiamiswimwear.com",
    Hosts::Www("omgmiamiswimwear.com")
);
instance!(
    PINUPGIRLCLOTHING,
    PINUPGIRLCLOTHING_STORE,
    "pinupgirlclothing",
    "Pinup Girl Clothing",
    "https://pinupgirlclothing.com",
    Hosts::Bare("pinupgirlclothing.com")
);
instance!(
    RAIDLONDON,
    RAIDLONDON_STORE,
    "raidlondon",
    "Raid London",
    "https://www.raidlondon.com",
    Hosts::Www("raidlondon.com")
);
instance!(
    UNIQUE_VINTAGE,
    UNIQUE_VINTAGE_STORE,
    "unique-vintage",
    "Unique Vintage",
    "https://www.unique-vintage.com",
    Hosts::Www("unique-vintage.com")
);
instance!(
    WINDSORSTORE,
    WINDSORSTORE_STORE,
    "windsorstore",
    "Windsor",
    "https://www.windsorstore.com",
    Hosts::Www("windsorstore.com")
);

#[derive(Debug, PartialEq)]
enum Target {
    Collection(String),
    Product(String),
}

/// The URL path below the store root, if the store accepts the URL's host.
fn store_path(hosts: &Hosts, url: &Url) -> Option<String> {
    if let Hosts::Configured(spec) = hosts {
        let rest = crate::sites::instances::below(url, spec)?;
        return Some(rest.split('?').next().unwrap_or_default().to_owned());
    }
    host_matches(hosts, url).then(|| url.path().to_owned())
}

fn host_matches(hosts: &Hosts, url: &Url) -> bool {
    if !crate::sites::plain(url) {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    match hosts {
        Hosts::Configured(_) => false,
        Hosts::Www(domain) => crate::sites::host_is(url, &[domain]),
        Hosts::Bare(domain) => host == *domain,
        Hosts::Sub(domain) => {
            host == *domain
                || host
                    .strip_suffix(domain)
                    .and_then(|s| s.strip_suffix('.'))
                    .is_some_and(|sub| {
                        !sub.is_empty() && sub.bytes().all(|b| b.is_ascii_lowercase())
                    })
        }
    }
}

/// `[\w-]+` at the start of `text`.
fn slug(text: &str) -> &str {
    let len = text
        .find(|c: char| !(crate::sites::media::word_char(c) || c == '-'))
        .unwrap_or(text.len());
    &text[..len]
}

/// An optional `/[a-z]{2}(?:-[^/?#]+)?` locale, then
/// `(/collections/[\w-]+)/?(?:$|[?#])` or
/// `((?:/collections/[\w-]+)?/products/[\w-]+)`.
fn target(hosts: &Hosts, url: &Url) -> Option<Target> {
    let path = store_path(hosts, url)?;
    let path = path.as_str();
    let candidates = std::iter::once(path).chain(locale(path));
    for path in candidates {
        if let Some(rest) = path.strip_prefix("/collections/") {
            let name = slug(rest);
            if !name.is_empty() && matches!(&rest[name.len()..], "" | "/") {
                return Some(Target::Collection(format!("/collections/{name}")));
            }
            if !name.is_empty()
                && let Some(product) = rest[name.len()..].strip_prefix("/products/")
                && !slug(product).is_empty()
            {
                return Some(Target::Product(format!(
                    "/collections/{name}/products/{}",
                    slug(product)
                )));
            }
        }
        if let Some(product) = path.strip_prefix("/products/")
            && !slug(product).is_empty()
        {
            return Some(Target::Product(format!("/products/{}", slug(product))));
        }
    }
    None
}

/// The path after a leading `/[a-z]{2}(?:-[^/?#]+)?` locale segment.
fn locale(path: &str) -> Option<&str> {
    let rest = path.strip_prefix('/')?;
    let (segment, _) = rest.split_once('/')?;
    let (lang, region) = segment.split_once('-').unwrap_or((segment, ""));
    let valid = lang.len() == 2
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && (region.is_empty() == !segment.contains('-'));
    valid.then(|| &rest[segment.len()..])
}

fn extract<'a>(
    site: &'static Site,
    root: &'static str,
    hosts: &Hosts,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    match target(hosts, &url) {
        Some(Target::Collection(path)) => crate::sites::once(async move {
            let meta = http.json(&http.api(&format!("{root}{path}.json"))?).await?;
            let Value::Object(meta) = meta else {
                return Err(error("Shopify collection response is not an object"));
            };
            let endpoint = format!("{root}{path}/products.json");
            Ok(crate::sites::paginate(Some(1u64), move |page| {
                let (endpoint, meta) = (endpoint.clone(), meta.clone());
                async move {
                    let Some(page) = page else {
                        return Ok(None);
                    };
                    let mut url = http.api(&endpoint)?;
                    url.query_pairs_mut().append_pair("page", &page.to_string());
                    let data = http.json(&url).await?;
                    let products = match data.get("products") {
                        Some(Value::Array(products)) if !products.is_empty() => products.clone(),
                        Some(Value::Array(_)) => return Ok(None),
                        _ => return Err(error("Shopify response has no products")),
                    };
                    let mut events = Vec::new();
                    for product in &products {
                        events.extend(crate::sites::post(
                            site,
                            http,
                            &COLLECTION,
                            product_items(product, &meta),
                        ));
                    }
                    Ok(Some((crate::sites::events(events), Some(page + 1))))
                }
            }))
        }),
        Some(Target::Product(path)) => crate::sites::once(async move {
            let data = http.json(&http.api(&format!("{root}{path}.json"))?).await?;
            let Some(Value::Object(mut product)) = data.get("product").cloned() else {
                return Err(error("Shopify response has no product"));
            };
            product.remove("image");
            let items = product_items(&Value::Object(product), &Map::new());
            Ok(crate::sites::events(crate::sites::post(
                site, http, &PRODUCT, items,
            )))
        }),
        None => crate::sites::fail(error("unsupported Shopify URL")),
    }
}

/// `items()` for one product: its images, numbered from one.
pub(crate) fn product_items(product: &Value, meta: &Map<String, Value>) -> Vec<Item> {
    let Some(mut product) = product.as_object().cloned() else {
        return Vec::new();
    };
    let images = match product.remove("images") {
        Some(Value::Array(images)) => images,
        _ => Vec::new(),
    };
    let product = Value::Object(product);
    let title = product
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let vendor = product
        .get("vendor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let pid = product
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    images
        .into_iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let Value::Object(mut image) = image else {
                return None;
            };
            let raw = image.get("src").and_then(Value::as_str)?.to_owned();
            let url = crate::sites::art::file_url(&raw, "https://cdn.shopify.com")?;
            nameext_into(&raw, &mut image);
            crate::sites::art::update(&mut image, meta);
            image.insert("product".into(), product.clone());
            image.insert("num".into(), (index + 1).into());
            let id = image
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            Some(
                Item::new(url, image)
                    .ids(pid.clone(), id)
                    .user(vendor.clone())
                    .caption(title.clone()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |hosts: &Hosts, raw: &str| target(hosts, &Url::parse(raw).unwrap());
        let nova = Hosts::Www("fashionnova.com");
        assert_eq!(
            t(
                &nova,
                "https://www.fashionnova.com/collections/mini-dresses/?page=1"
            ),
            Some(Target::Collection("/collections/mini-dresses".into()))
        );
        assert_eq!(
            t(
                &nova,
                "https://www.fashionnova.com/en-gb/collections/a/products/b-c?v=1"
            ),
            Some(Target::Product("/collections/a/products/b-c".into()))
        );
        assert_eq!(
            t(&nova, "https://fashionnova.com/de/products/x_y/extra"),
            Some(Target::Product("/products/x_y".into()))
        );
        let lounge = Hosts::Sub("loungeunderwear.com");
        assert!(t(&lounge, "https://uk.loungeunderwear.com/products/a").is_some());
        let bare = Hosts::Bare("modcloth.com");
        assert!(t(&bare, "https://modcloth.com/products/a").is_some());
        assert_eq!(t(&bare, "https://www.modcloth.com/products/a"), None);
        for raw in [
            "https://www.fashionnova.com/collections/a/b",
            "https://www.fashionnova.com/abc/products/a",
            "https://www.fashionnova.com/products/",
            "https://fashionnova.com.evil/products/a",
            "https://evilfashionnova.com/products/a",
            "https://fashionnova.com:81/products/a",
            "https://u@fashionnova.com/products/a",
        ] {
            assert_eq!(t(&nova, raw), None, "{raw}");
        }
        assert_eq!(
            t(&lounge, "https://a.b.loungeunderwear.com/products/a"),
            None
        );
    }

    #[tokio::test]
    async fn extracts_collection() {
        let product = |id: u64| {
            json!({"id": id, "title": format!("Dress {id}"), "vendor": "Nova", "images": [
                {"id": id * 10, "src": format!("https://cdn.shopify.com/s/files/{id}_a.jpg?v=1")},
                {"id": id * 10 + 1, "src": format!("https://cdn.shopify.com/s/files/{id}_b.png?v=1")}]})
        };
        let (root, server) = serve(vec![
            json(json!({"collection": {"title": "Minis", "id": 5}})),
            json(json!({"products": [product(1)]})),
            json(json!({"products": [product(2)]})),
            json(json!({"products": []})),
        ]);
        let http = http("fashionnova", root);
        let files = media((FASHIONNOVA.extract)(
            &http,
            Url::parse("https://www.fashionnova.com/collections/minis").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        let k = keywords(&files[3]);
        assert_eq!(k["collection"]["title"], "Minis");
        assert_eq!(k["product"]["title"], "Dress 2");
        assert!(k["product"].get("images").is_none());
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "2_b");
        let (file, dir) = crate::sites::default_templates(&files[3]).unwrap();
        assert_eq!(file.render(&files[3]).unwrap(), "Dress 2_02_21.png");
        assert_eq!(
            dir.render(&files[3]).unwrap().unwrap(),
            std::path::PathBuf::from("fashionnova/Minis")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["10"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /collections/minis.json "));
        assert!(requests[1].starts_with("GET /collections/minis/products.json?page=1 "));
        assert!(requests[3].starts_with("GET /collections/minis/products.json?page=3 "));
    }

    #[tokio::test]
    async fn extracts_product() {
        let (root, server) = serve(vec![json(json!({"product": {"id": 1, "title": "Cam",
            "image": {"src": "x"},
            "images": [{"id": 9, "src": "https://cdn.shopify.com/s/files/cam.jpg"}]}}))]);
        let http = http("michaelscameras", root);
        let files = media((MICHAELSCAMERAS.extract)(
            &http,
            Url::parse("https://michaels.com.au/collections/c/products/cam").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert!(keywords(&files[0])["product"].get("image").is_none());
        assert_eq!(keywords(&files[0])["category"], "michaelscameras");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("michaelscameras/Products")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /collections/c/products/cam.json "));
    }

    #[tokio::test]
    async fn configured_instance_uses_its_root() {
        crate::sites::instances::define(
            "shopify",
            "shopify-configured",
            json!({"root": "https://shop.test/store", "pattern": r"(?:[a-z]+\.)?shop\.test/store"}),
        );
        let url = Url::parse("https://eu.shop.test/store/en-gb/products/cam?v=1").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "shopify-configured");
        assert!(
            crate::sites::find(&Url::parse("https://shop.test/products/cam").unwrap()).is_none()
        );
        let (root, server) = serve(vec![json(json!({"product": {"id": 1, "title": "Cam",
            "images": [{"id": 9, "src": "https://cdn.shopify.com/s/files/cam.jpg"}]}}))]);
        let http = http("shopify-configured", root);
        let files = media((site.extract)(&http, url)).await;
        assert_eq!(keywords(&files[0])["category"], "shopify-configured");
        assert_eq!(keywords(&files[0])["subcategory"], "product");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /store/products/cam.json "),
            "{}",
            requests[0]
        );
    }
}
