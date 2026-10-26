//! Photo galleries and image boards of adult and general-interest sites.
use crate::sites::{Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

mod adultempire;
mod ahottie;
mod eightmuses;
mod eporner;
mod everia;
mod fapachi;
mod fapello;
mod fuskator;
mod girlsreleased;
mod hentaicosplays;
mod pornhub;
mod sankakucomplex;
mod xasiat;
mod xhamster;
mod xvideos;

pub(super) static SITES: &[&Site] = &[
    &eporner::SITE,
    &xvideos::SITE,
    &xhamster::SITE,
    &xasiat::SITE,
    &pornhub::SITE,
    &sankakucomplex::SITE,
    &adultempire::SITE,
    &ahottie::SITE,
    &fapachi::SITE,
    &hentaicosplays::HENTAICOSPLAY,
    &hentaicosplays::HENTAIIMG,
    &hentaicosplays::PORNIMAGE,
    &girlsreleased::SITE,
    &fuskator::SITE,
    &eightmuses::SITE,
    &everia::SITE,
    &fapello::SITE,
];

/// `GalleryExtractor.items`: files numbered under `num` with a `count`.
pub(super) fn numbered(
    site: &Site,
    http: &Http,
    route: &'static Route,
    data: &Map<String, Value>,
    images: Vec<(String, Option<Map<String, Value>>)>,
    post_id: &str,
) -> Vec<crate::event::ExtractionEvent> {
    let count = match data.get("count").and_then(Value::as_i64) {
        Some(count) => usize::try_from(count).unwrap_or(0),
        None => images.len(),
    };
    let items = images
        .into_iter()
        .take(count)
        .enumerate()
        .filter_map(|(index, (raw, image))| {
            let url = Url::parse(raw.trim()).ok().filter(crate::sites::plain)?;
            let mut fields = data.clone();
            fields.entry("count").or_insert(count.into());
            fields.insert("num".into(), (index + 1).into());
            let named = image.as_ref().is_some_and(|i| i.contains_key("extension"));
            if let Some(image) = image {
                fields.extend(image);
            }
            if !named {
                crate::sites::gdl::nameext_into(url.as_str(), &mut fields);
            }
            Some(Item::new(url, fields).ids(post_id, (index + 1).to_string()))
        })
        .collect();
    crate::sites::post(site, http, route, items)
}

/// Queue child URLs (upstream `Message.Queue`), in order.
pub(super) fn queue<'a>(
    http: &'a Http,
    site: &'static Site,
    route: &'static Route,
    children: Vec<(Url, Map<String, Value>)>,
) -> crate::sites::Events<'a> {
    use futures_util::StreamExt;
    futures_util::stream::iter(children)
        .map(move |(url, data)| {
            crate::sites::queue(http, site.category, route.subcategory, url, &data)
        })
        .flatten()
        .boxed()
}
