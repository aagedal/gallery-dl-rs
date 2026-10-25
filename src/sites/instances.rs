//! Configured and prefixed instances of upstream `BaseExtractor` families,
//! mirroring `BaseExtractor.update` and `_init_category` in
//! gallery_dl/extractor/common.py.
//!
//! * `extractor.<basecategory>.<category> = {"root": ..., "pattern": ...}`
//!   adds an instance named `<category>` next to the family's built-in
//!   instances. `pattern` (a regular expression matched at the start of the
//!   URL after `http(s)://`) defaults to the escaped root without scheme.
//!   Naming a built-in instance replaces its root and pattern.
//! * `<basecategory>:https://host/...` selects the family for any host; the
//!   instance's category is the host and its root `https://host`.
//!
//! Instances live for the rest of the process (leaked once each). Every
//! instance is its own category, so its HTTP state, cookies and settings are
//! scoped to it; settings fall back to the `<basecategory>` scope.
//!
//! Families resolve a configured category's root through [`configured`] and
//! receive URLs rewritten onto that root, so their own URL parsing applies
//! unchanged: `https://alias.example/x` matched by an instance pattern is
//! extracted as `{root}/x`.
use super::{Events, Http, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::sync::RwLock;
use url::Url;

/// Maximum length of a configured instance pattern.
const MAX_PATTERN: usize = 1024;
/// Compiled-size bound for configured patterns.
const MAX_PATTERN_SIZE: usize = 1 << 20;
/// Maximum length of a configured root.
const MAX_ROOT: usize = 512;
/// Maximum number of instances (configured plus prefixed) per process.
const MAX_INSTANCES: usize = 1024;

/// One upstream `BaseExtractor` subclass with configurable instances.
pub struct Family {
    /// Upstream `basecategory`: the shared settings scope, the key holding
    /// instance definitions and the input prefix.
    pub basecategory: &'static str,
    /// Built-in instances, all part of the static registry.
    pub builtin: &'static [&'static Site],
    /// Other registry categories whose upstream class shares the
    /// `basecategory` settings scope without being an instance (e.g.
    /// paheal for shimmie2).
    pub related: &'static [&'static str],
    /// Routes and flags for configured instances (usually a built-in one).
    pub template: &'static Site,
    /// Whether the family's extractors accept a URL on `category`'s root.
    pub matches: fn(&'static str, &Url) -> bool,
    /// Extract a URL on `category`'s root.
    pub extract: for<'a> fn(&'static str, &'a Http, Url) -> Events<'a>,
}

impl std::fmt::Debug for Family {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Family")
            .field("basecategory", &self.basecategory)
            .finish_non_exhaustive()
    }
}

/// Every family, in registry order.
pub fn families() -> &'static [&'static Family] {
    static ALL: std::sync::OnceLock<Vec<&'static Family>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        [
            super::imageboards::FAMILIES,
            super::boorus::FAMILIES,
            super::media::FAMILIES,
            super::filehosts::FAMILIES,
            super::manga::FAMILIES,
            super::art::FAMILIES,
            super::social::FAMILIES,
            super::adult::FAMILIES,
        ]
        .concat()
    })
}

/// The family with this `basecategory`.
pub fn family(basecategory: &str) -> Option<&'static Family> {
    families()
        .iter()
        .copied()
        .find(|family| family.basecategory == basecategory)
}

/// A validated `extractor.<basecategory>.<category>` instance object.
#[derive(Clone, Debug)]
pub struct InstanceDef {
    pub basecategory: String,
    pub category: String,
    /// `scheme://authority[/path]` without a trailing slash.
    pub root: String,
    pattern: Option<regex::Regex>,
    /// The remaining instance fields (upstream `config_instance`).
    pub info: Map<String, Value>,
}

/// Validate one instance object; `Ok(None)` when `value` is not an instance
/// definition (an object without `root`).
pub fn parse_def(basecategory: &str, category: &str, value: &Value) -> Result<Option<InstanceDef>> {
    let Some(fields) = value
        .as_object()
        .filter(|fields| fields.contains_key("root"))
    else {
        return Ok(None);
    };
    let family = family(basecategory).ok_or_else(|| error("unknown instance family"))?;
    valid_category(category)?;
    if let Some(site) = super::sites().iter().find(|site| site.category == category)
        && !family.builtin.iter().any(|b| std::ptr::eq(*b, *site))
    {
        return Err(error(
            "configured instance category is already used by another extractor",
        ));
    }
    if crate::extract::is_dedicated_category(category) || family_scope(category, basecategory) {
        return Err(error(
            "configured instance category is already used by another extractor",
        ));
    }
    let root = match fields.get("root") {
        Some(Value::String(root)) => parse_root(root)?,
        _ => {
            return Err(error(
                "configured instance root must be an http(s) URL string",
            ));
        }
    };
    let pattern = match fields.get("pattern") {
        None | Some(Value::Null) => None,
        Some(Value::String(pattern)) if pattern.is_empty() => None,
        Some(Value::String(pattern)) => Some(compile(pattern)?),
        Some(_) => return Err(error("configured instance pattern must be a string")),
    };
    let info = fields
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "root" | "pattern"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok(Some(InstanceDef {
        basecategory: basecategory.to_owned(),
        category: category.to_owned(),
        root,
        pattern,
        info,
    }))
}

/// Whether `name` is the base scope of another family than `basecategory`.
fn family_scope(name: &str, basecategory: &str) -> bool {
    name != basecategory && family(name).is_some()
}

fn valid_category(category: &str) -> Result<()> {
    if category.is_empty()
        || category.len() > 64
        || !category
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        || matches!(category, "." | "..")
    {
        return Err(error(
            "configured instance category must be 1-64 ASCII letters, digits, '.', '-' or '_'",
        ));
    }
    Ok(())
}

/// Whether `name` can be the category of a `<basecategory>:https://host/`
/// input: a host name with an optional port.
pub fn host_scope(name: &str) -> bool {
    let (host, port) = name.split_once(':').unwrap_or((name, "0"));
    (1..=253).contains(&host.len())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !host.starts_with('.')
        && port.parse::<u16>().is_ok()
}

/// An instance root: http(s), a host, no credentials, query or fragment.
fn parse_root(raw: &str) -> Result<String> {
    let invalid = || {
        error(
            "configured instance root must be an http(s) URL without credentials, query or fragment",
        )
    };
    if raw.len() > MAX_ROOT {
        return Err(invalid());
    }
    let url = Url::parse(raw).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn compile(pattern: &str) -> Result<regex::Regex> {
    if pattern.len() > MAX_PATTERN {
        return Err(error("configured instance pattern exceeds 1024 characters"));
    }
    regex::RegexBuilder::new(&format!("^(?:{pattern})"))
        .size_limit(MAX_PATTERN_SIZE)
        .dfa_size_limit(MAX_PATTERN_SIZE)
        .build()
        .map_err(|_| error("configured instance pattern is not a supported regular expression"))
}

/// One configured or prefixed instance.
pub struct Instance {
    pub category: &'static str,
    pub family: &'static Family,
    /// `scheme://authority[/path]` without a trailing slash.
    pub root: &'static str,
    /// The root without its scheme: `host[:port][/path]`.
    pub spec: &'static str,
    /// `[spec]`, for host lists.
    pub specs: &'static [&'static str],
    /// The root's host, without port.
    pub host: &'static str,
    pattern: Option<regex::Regex>,
    /// Instance fields besides `root` and `pattern` (upstream
    /// `config_instance`).
    pub info: Map<String, Value>,
    /// Selected with a `<basecategory>:` prefix.
    pub prefixed: bool,
    /// Registered after the program resolved its per-category settings;
    /// such instances use their base category's settings.
    pub late: bool,
    pub site: Site,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("category", &self.category)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Instance {
    /// The URL rewritten onto this instance's root, if the instance pattern
    /// matches it.
    fn canonical(&self, url: &Url) -> Option<Url> {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let full = url.as_str();
        let start = full.find("://")? + 3;
        let end = full.find('#').unwrap_or(full.len());
        let text = &full[start..end];
        let matched = match &self.pattern {
            Some(pattern) => pattern.find(text)?.end(),
            None => {
                let head = text.get(..self.spec.len())?;
                head.eq_ignore_ascii_case(self.spec)
                    .then_some(self.spec.len())?
            }
        };
        let rest = &text[matched..];
        if !(rest.is_empty() || rest.starts_with(['/', '?'])) {
            return None;
        }
        let canonical = Url::parse(&format!("{}{rest}", self.root)).ok()?;
        (self.family.matches)(self.category, &canonical).then_some(canonical)
    }

    /// Upstream `config_instance`: a field of the instance object.
    pub fn config(&self, key: &str) -> Option<&Value> {
        self.info.get(key)
    }
}

static REGISTRY: RwLock<Vec<&'static Instance>> = RwLock::new(Vec::new());
static FROZEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn registry() -> std::sync::RwLockReadGuard<'static, Vec<&'static Instance>> {
    REGISTRY.read().unwrap_or_else(|e| e.into_inner())
}

/// Every registered instance: prefixed ones first, then configured ones in
/// definition order.
pub fn all() -> Vec<&'static Instance> {
    let registry = registry();
    let mut all: Vec<_> = registry.iter().copied().filter(|i| i.prefixed).collect();
    all.extend(registry.iter().copied().filter(|i| !i.prefixed));
    all
}

/// The registered instance named `category`.
pub fn get(category: &str) -> Option<&'static Instance> {
    registry().iter().copied().find(|i| i.category == category)
}

/// The registered instance named `category` of `family`, for the family's
/// root and host lookups.
pub fn configured(family: &Family, category: &str) -> Option<&'static Instance> {
    get(category).filter(|i| std::ptr::eq(i.family, family))
}

/// The instance handling `url`, and the URL on its root.
pub fn lookup(url: &Url) -> Option<(&'static Instance, Url)> {
    all()
        .into_iter()
        .find_map(|instance| Some((instance, instance.canonical(url)?)))
}

/// Whether a built-in category was replaced by a configured instance.
pub fn overridden(category: &str) -> bool {
    get(category).is_some()
}

/// The upstream `basecategory` of a registry category, built-in or not.
pub fn basecategory(category: &str) -> Option<&'static str> {
    if let Some(instance) = get(category) {
        return Some(instance.family.basecategory);
    }
    families()
        .iter()
        .find(|family| {
            family.builtin.iter().any(|s| s.category == category)
                || family.related.contains(&category)
        })
        .map(|family| family.basecategory)
}

/// Mark the instances registered so far as having their own settings.
pub fn freeze() {
    FROZEN.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

fn instance_site_extract<'a>(http: &'a Http, url: Url) -> Events<'a> {
    match lookup(&url) {
        Some((instance, url)) => (instance.family.extract)(instance.category, http, url),
        None => super::fail(error("unsupported instance URL")),
    }
}

fn new_instance(
    family: &'static Family,
    category: &str,
    root: &str,
    pattern: Option<regex::Regex>,
    info: Map<String, Value>,
    prefixed: bool,
) -> Result<&'static Instance> {
    let parsed = Url::parse(root).map_err(|_| error("invalid instance root"))?;
    let host = leak(parsed.host_str().unwrap_or_default());
    let root = leak(root);
    let spec = &root[root.find("://").map_or(0, |i| i + 3)..];
    let category = leak(category);
    let template = family.template;
    Ok(Box::leak(Box::new(Instance {
        category,
        family,
        root,
        spec,
        specs: Box::leak(Box::new([spec])),
        host,
        pattern,
        info,
        prefixed,
        late: FROZEN.load(std::sync::atomic::Ordering::SeqCst),
        site: Site {
            category,
            name: category,
            matches: |url| lookup(url).is_some(),
            extract: instance_site_extract,
            routes: template.routes,
            api_key: template.api_key,
            cookies: template.cookies.map(|_| host),
        },
    })))
}

/// Register configured instances; categories already registered are kept.
pub fn register(defs: &[InstanceDef]) -> Result<()> {
    let mut registry = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    for def in defs {
        if registry.iter().any(|i| i.category == def.category) {
            continue;
        }
        if registry.len() >= MAX_INSTANCES {
            return Err(error("too many extractor instances"));
        }
        let family = family(&def.basecategory).ok_or_else(|| error("unknown instance family"))?;
        registry.push(new_instance(
            family,
            &def.category,
            &def.root,
            def.pattern.clone(),
            def.info.clone(),
            false,
        )?);
    }
    Ok(())
}

/// Handle a `<basecategory>:http(s)://host/...` input: registers the host
/// as an instance of the family and returns the plain URL. `None` when the
/// input has no family prefix.
pub fn prefixed_input(value: &str) -> Option<Result<Url>> {
    let (prefix, target) = value.split_once(':')?;
    let family = family(prefix)?;
    let lower = target.get(..8)?.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    Some(register_prefixed(family, target))
}

fn register_prefixed(family: &'static Family, target: &str) -> Result<Url> {
    let url = crate::extract::http_url(target)?;
    let authority = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let root = format!("{}://{authority}", url.scheme());
    // A built-in instance of the family with this category and root.
    if family
        .builtin
        .iter()
        .any(|site| site.category == authority && (site.matches)(&url))
    {
        return Ok(url);
    }
    let mut registry = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.iter().find(|i| i.category == authority) {
        if std::ptr::eq(existing.family, family) && existing.root == root {
            return Ok(url);
        }
        return Err(error(
            "prefixed instance host conflicts with another instance",
        ));
    }
    if super::sites().iter().any(|site| site.category == authority)
        || crate::extract::is_dedicated_category(&authority)
    {
        return Err(error(
            "prefixed instance host conflicts with another extractor",
        ));
    }
    if registry.len() >= MAX_INSTANCES {
        return Err(error("too many extractor instances"));
    }
    valid_category(authority.split(':').next().unwrap_or_default())?;
    registry.push(new_instance(
        family,
        &authority,
        &root,
        None,
        Map::new(),
        true,
    )?);
    Ok(url)
}

/// The URL upstream gallery-dl needs for a registry URL: prefixed instances
/// keep their `<basecategory>:` prefix.
pub fn upstream_input(url: &Url) -> String {
    match lookup(url) {
        Some((instance, _)) if instance.prefixed => {
            format!("{}:{}", instance.family.basecategory, url.as_str())
        }
        _ => url.as_str().to_owned(),
    }
}

/// [`super::plain`], or a credential-free URL on `root`'s origin, whose port
/// a configured instance may name.
pub(crate) fn plain_or_root(url: &Url, root: &str) -> bool {
    super::plain(url)
        || (matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && Url::parse(root).is_ok_and(|root| root.origin() == url.origin()))
}

/// `host[:port][/prefix]`-style matching for instance roots: the URL's
/// path and query below `spec`, accepting the URL's port only when `spec`
/// names it. Rejects credentials and non-HTTP schemes.
pub(crate) fn below(url: &Url, spec: &str) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let (root_authority, prefix) = match spec.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (spec, String::new()),
    };
    if !authority.eq_ignore_ascii_case(root_authority) {
        return None;
    }
    let mut rest = url.path().to_owned();
    if let Some(query) = url.query() {
        rest.push('?');
        rest.push_str(query);
    }
    if prefix.is_empty() {
        return Some(rest);
    }
    let tail = rest.strip_prefix(&prefix)?;
    (tail.is_empty() || tail.starts_with(['/', '?'])).then(|| tail.to_owned())
}

/// Register a configured instance from its JSON object, for tests.
#[cfg(test)]
pub(crate) fn define(basecategory: &str, category: &str, info: Value) -> &'static Instance {
    let def = parse_def(basecategory, category, &info)
        .expect("valid instance")
        .expect("instance object");
    register(std::slice::from_ref(&def)).expect("registered");
    get(category).expect("instance")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn validates_instance_objects() {
        let def = |category: &str, value: Value| parse_def("shimmie2", category, &value);
        assert!(def("x", json!({"pattern": "x"})).unwrap().is_none());
        assert!(def("x", json!("text")).unwrap().is_none());
        let ok = def(
            "okbooru",
            json!({"root": "https://ok.test/", "quote": "\""}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.root, "https://ok.test");
        assert_eq!(ok.info["quote"], "\"");
        for root in [
            json!("ftp://ok.test"),
            json!("https://user:pw@ok.test"),
            json!("https://ok.test/?q=1"),
            json!("https://ok.test/#x"),
            json!("ok.test"),
            json!(null),
            json!(1),
        ] {
            assert!(def("bad", json!({"root": root})).is_err(), "{root}");
        }
        for pattern in [json!("(unclosed"), json!(r"(?=lookahead)"), json!(1)] {
            assert!(
                def(
                    "bad",
                    json!({"root": "https://ok.test", "pattern": pattern})
                )
                .is_err()
            );
        }
        let long = "a".repeat(MAX_PATTERN + 1);
        assert!(def("bad", json!({"root": "https://ok.test", "pattern": long})).is_err());
        // Categories of other extractors and families, and unsafe names.
        for category in ["twitter", "yandere", "paheal", "", "a/b", "..", "a b"] {
            assert!(
                def(category, json!({"root": "https://ok.test"})).is_err(),
                "{category}"
            );
        }
        // A built-in instance of the family may be replaced.
        assert!(
            def("vidyapics", json!({"root": "https://vidya.test"}))
                .unwrap()
                .is_some()
        );
        assert!(parse_def("nosuchfamily", "x", &json!({"root": "https://ok.test"})).is_err());
    }

    #[test]
    fn routes_configured_patterns_and_default_roots() {
        let patterned = define(
            "shimmie2",
            "routing-patterned",
            json!({"root": "https://pat.test/booru", "pattern": r"(?:www\.)?pat\.test/booru|alias\.test"}),
        );
        let plain = define(
            "shimmie2",
            "routing-plain",
            json!({"root": "http://plain.test:8080"}),
        );
        let found = |raw: &str| lookup(&url(raw)).map(|(i, u)| (i.category, u.to_string()));
        assert_eq!(
            found("https://www.pat.test/booru/post/view/1"),
            Some((
                patterned.category,
                "https://pat.test/booru/post/view/1".into()
            ))
        );
        assert_eq!(
            found("http://alias.test/post/list/tag/2#top"),
            Some((
                patterned.category,
                "https://pat.test/booru/post/list/tag/2".into()
            ))
        );
        // The pattern must end at a path boundary, and the family must
        // accept the remaining path.
        assert_eq!(found("https://alias.test.evil/post/view/1"), None);
        assert_eq!(found("https://alias.test/other/1"), None);
        assert_eq!(found("https://user@alias.test/post/view/1"), None);
        assert_eq!(
            found("http://plain.test:8080/post/view/3"),
            Some((plain.category, "http://plain.test:8080/post/view/3".into()))
        );
        assert_eq!(found("http://plain.test/post/view/3"), None);
        assert_eq!(found("http://plain.test:8081/post/view/3"), None);
        let site = crate::sites::find(&url("https://alias.test/post/view/1")).unwrap();
        assert_eq!(site.category, "routing-patterned");
        assert!(std::ptr::eq(
            crate::sites::site("routing-plain").unwrap(),
            &plain.site
        ));
        assert!(crate::sites::categories().any(|c| c == "routing-plain"));
        assert_eq!(basecategory("routing-plain"), Some("shimmie2"));
        assert_eq!(basecategory("cavemanon"), Some("shimmie2"));
        assert_eq!(basecategory("4chan"), None);
        assert_eq!(plain.site.cookies, None);
        assert_eq!(patterned.spec, "pat.test/booru");
        assert_eq!(plain.host, "plain.test");
    }

    #[test]
    fn prefixed_inputs_register_host_instances() {
        let parsed = prefixed_input("shimmie2:https://prefixed.test/post/view/7")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.as_str(), "https://prefixed.test/post/view/7");
        let instance = get("prefixed.test").unwrap();
        assert!(instance.prefixed);
        assert_eq!(instance.root, "https://prefixed.test");
        assert_eq!(
            crate::sites::find(&parsed).map(|site| site.category),
            Some("prefixed.test")
        );
        assert_eq!(
            upstream_input(&parsed),
            "shimmie2:https://prefixed.test/post/view/7"
        );
        // The same host again is the same instance; inputs without a known
        // family prefix or an http(s) target are not instances.
        assert!(prefixed_input("shimmie2:https://prefixed.test/post/list/a/1").is_some());
        assert_eq!(
            all()
                .iter()
                .filter(|i| i.category == "prefixed.test")
                .count(),
            1
        );
        assert!(prefixed_input("nosuchfamily:https://x.test/").is_none());
        assert!(prefixed_input("shimmie2:file:///etc/passwd").is_none());
        assert!(
            prefixed_input("shimmie2:https://u:p@creds.test/post/view/1")
                .unwrap()
                .is_err()
        );
        assert!(
            prefixed_input("shimmie2:https://twitter/post/view/1")
                .unwrap()
                .is_err()
        );
        assert!(host_scope("prefixed.test"));
        assert!(host_scope("127.0.0.1:8080"));
        assert!(!host_scope("a/b"));
        assert!(!host_scope("host:port"));
    }
}
