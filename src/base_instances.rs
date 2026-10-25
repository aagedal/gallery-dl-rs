//! User-configured instances and URL prefixes of upstream's native
//! `BaseExtractor` families (Danbooru, E621 and mastodon).
//!
//! Mirrors gallery_dl/extractor/common.py `BaseExtractor.update` and
//! `_init_category`: `extractor.<basecategory>.<category>` objects with a
//! `root` add an instance whose `pattern` (default: the escaped root host)
//! selects input URLs, and `<basecategory>:https://HOST/...` inputs use the
//! family for any host, with the host (and port) as category and the URL's
//! scheme and host as root.
//!
//! Instances live for the rest of the process: configured ones are
//! registered once from the configuration, prefixed ones when an input first
//! names their host, so later child URLs on that host route to them too.
use crate::{Result, error};
use std::sync::RwLock;
use url::Url;

/// Upstream `basecategory` of a native instance family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Base {
    Danbooru,
    E621,
    Mastodon,
}

impl Base {
    pub const ALL: [Self; 3] = [Self::Danbooru, Self::E621, Self::Mastodon];

    /// Upstream `basecategory`: the configuration scope shared by the
    /// family's instances and the spelling of its URL prefix.
    pub fn scope(self) -> &'static str {
        match self {
            Self::Danbooru => "Danbooru",
            Self::E621 => "E621",
            Self::Mastodon => "mastodon",
        }
    }

    /// The (lowercase) URL scheme a prefixed input is kept under.
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Danbooru => "danbooru",
            Self::E621 => "e621",
            Self::Mastodon => "mastodon",
        }
    }

    /// The family of an input prefix: upstream's exact `basecategory`
    /// spelling, or its lowercase form.
    pub fn from_prefix(prefix: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|base| prefix == base.scope() || prefix == base.scheme())
    }

    fn from_scheme(scheme: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|base| scheme == base.scheme())
    }
}

/// A validated `extractor.<basecategory>.<category>` instance definition.
#[derive(Clone, PartialEq, Eq)]
pub struct Definition {
    pub base: Base,
    pub category: String,
    /// The instance origin (`scheme://host[:port]/`).
    pub root: Url,
    pub pattern: Option<String>,
    /// Mastodon `access-token`, used after the scoped setting and the cache
    /// (upstream `config_instance`).
    pub access_token: Option<String>,
    /// Mastodon OAuth application for `oauth:mastodon:INSTANCE`.
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Definition")
            .field("base", &self.base)
            .field("category", &self.category)
            .field("root", &self.root.as_str())
            .field("pattern", &self.pattern)
            .finish_non_exhaustive()
    }
}

/// Bound on configured instance patterns, before compilation.
const MAX_PATTERN: usize = 1024;

/// Whether `name` can be a configured instance category: a bounded label
/// usable as a configuration scope and path component.
fn valid_category(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !name.starts_with('.')
        && !name.ends_with('.')
        // `CATEGORY.CLASS` names are Danbooru/e621 class settings keys.
        && ![".tag", ".artist", ".artist-search"]
            .iter()
            .any(|class| name.ends_with(class))
}

/// Names that already have a meaning as categories or scopes.
fn reserved(name: &str) -> bool {
    crate::extract::dedicated_category(name)
        || crate::sites::is_category(name)
        || crate::booru::Site::ALL
            .iter()
            .any(|site| site.category() == name)
        || Base::ALL.iter().any(|base| base.scope() == name)
        || matches!(
            name,
            "oauth" | "generic" | "recursive" | "directlink" | "postprocessor"
        )
}

/// A configured instance root: an http(s) origin without credentials.
fn parse_root(raw: &str) -> Result<Url> {
    let invalid = || {
        error(
            "native configuration instance root must be an http(s) origin without credentials, path, query or fragment",
        )
    };
    if raw.len() > 2048 {
        return Err(invalid());
    }
    let root = Url::parse(raw).map_err(|_| invalid())?;
    if !matches!(root.scheme(), "http" | "https")
        || root.host_str().is_none_or(str::is_empty)
        || !root.username().is_empty()
        || root.password().is_some()
        || root.path() != "/"
        || root.query().is_some()
        || root.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(root)
}

fn compile(pattern: &str) -> Result<regex::Regex> {
    regex::RegexBuilder::new(&format!("^(?:https?://)?(?:{pattern})$"))
        .size_limit(1 << 20)
        .build()
        .map_err(|_| {
            error("native configuration instance pattern is not a supported regular expression")
        })
}

impl Definition {
    /// Parse `extractor.<base>.<category>`; `Ok(None)` for values upstream
    /// does not treat as an instance (anything but an object with `root`).
    pub fn parse(base: Base, category: &str, value: &serde_json::Value) -> Result<Option<Self>> {
        let Some(fields) = value
            .as_object()
            .filter(|fields| fields.contains_key("root"))
        else {
            return Ok(None);
        };
        if !valid_category(category) || reserved(category) {
            return Err(error(
                "native configuration instance names must be new 1-64 character categories of ASCII letters, digits, '.', '-' or '_'",
            ));
        }
        let string = |key: &str| -> Result<Option<String>> {
            match fields.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(value)) if value.len() <= 4096 => {
                    Ok(Some(value.clone()))
                }
                Some(_) => Err(error(format!(
                    "native configuration instance {key} must be a string"
                ))),
            }
        };
        let mastodon_keys: &[&str] = if base == Base::Mastodon {
            &["access-token", "client-id", "client-secret"]
        } else {
            &[]
        };
        if fields.keys().any(|key| {
            !matches!(key.as_str(), "root" | "pattern") && !mastodon_keys.contains(&key.as_str())
        }) {
            return Err(error("unsupported native configuration instance option"));
        }
        let root = parse_root(
            &string("root")?
                .ok_or_else(|| error("native configuration instance root must be a string"))?,
        )?;
        let pattern = string("pattern")?.filter(|pattern| !pattern.is_empty());
        if let Some(pattern) = &pattern {
            if pattern.len() > MAX_PATTERN {
                return Err(error("native configuration instance pattern is too long"));
            }
            compile(pattern)?;
        }
        Ok(Some(Self {
            base,
            category: category.to_owned(),
            root,
            pattern,
            access_token: string("access-token")?,
            client_id: string("client-id")?,
            client_secret: string("client-secret")?,
        }))
    }
}

/// `host[:port]` of an http(s) URL (upstream's `instance`/prefixed category).
fn netloc(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// A registered instance; references stay valid for the process.
pub struct Instance {
    pub base: Base,
    pub category: &'static str,
    /// The API origin (`scheme://host[:port]/`).
    pub root: Url,
    /// `scheme://host[:port]` of [`Instance::root`], for root-relative files.
    pub origin: &'static str,
    /// `host[:port]` of the root: upstream's Mastodon `instance` and
    /// credential-cache key.
    pub netloc: &'static str,
    /// Upstream `static1.{root host}` for hidden e621-dialect files.
    pub static_host: &'static str,
    /// `CATEGORY.SUBCATEGORY` keys of Danbooru/e621 class scopes.
    pub class_keys: [&'static str; 3],
    /// Added by a `<basecategory>:URL` input rather than configuration.
    pub prefixed: bool,
    pattern: Option<regex::Regex>,
    pub access_token: Option<&'static str>,
    pub client_id: Option<&'static str>,
    pub client_secret: Option<&'static str>,
}

impl PartialEq for Instance {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for Instance {}
impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("base", &self.base)
            .field("category", &self.category)
            .field("root", &self.root.as_str())
            .field("prefixed", &self.prefixed)
            .finish_non_exhaustive()
    }
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

impl Instance {
    fn new(
        base: Base,
        category: &str,
        root: Url,
        pattern: Option<regex::Regex>,
        prefixed: bool,
    ) -> Self {
        let netloc = netloc(&root).unwrap_or_default();
        let category = leak(category.to_owned());
        Self {
            base,
            category,
            origin: leak(root.as_str().trim_end_matches('/').to_owned()),
            static_host: leak(format!("static1.{}", root.host_str().unwrap_or_default())),
            class_keys: ["artist", "artist-search", "tag"]
                .map(|class| leak(format!("{category}.{class}"))),
            netloc: leak(netloc),
            root,
            prefixed,
            pattern,
            access_token: None,
            client_id: None,
            client_secret: None,
        }
    }

    /// Whether an http(s) URL belongs to this instance: upstream's pattern
    /// (`(?:https?://)?(?:PATTERN)` up to the path), or the root's exact
    /// `host[:port]` without one.
    pub fn matches(&self, url: &Url) -> bool {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return false;
        }
        let Some(netloc) = netloc(url) else {
            return false;
        };
        match &self.pattern {
            Some(pattern) => pattern.is_match(&format!("{}://{netloc}", url.scheme())),
            None => netloc == self.netloc,
        }
    }

    /// The `tag`/`artist`/`artist-search` settings key of this instance.
    pub fn class_key(&self, subcategory: &str) -> Option<&'static str> {
        ["artist", "artist-search", "tag"]
            .iter()
            .position(|class| *class == subcategory)
            .map(|index| self.class_keys[index])
    }
}

static REGISTRY: RwLock<Vec<&'static Instance>> = RwLock::new(Vec::new());

fn registry() -> std::sync::RwLockReadGuard<'static, Vec<&'static Instance>> {
    REGISTRY
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Register configured instances (done once, from the configuration). A
/// later definition of an already registered category replaces it.
pub fn register(definitions: &[Definition]) {
    let mut registry = REGISTRY
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for definition in definitions {
        let pattern = definition
            .pattern
            .as_deref()
            .map(|pattern| compile(pattern).expect("validated instance pattern"));
        let mut instance = Instance::new(
            definition.base,
            &definition.category,
            definition.root.clone(),
            pattern,
            false,
        );
        instance.access_token = definition.access_token.clone().map(leak);
        instance.client_id = definition.client_id.clone().map(leak);
        instance.client_secret = definition.client_secret.clone().map(leak);
        let instance: &'static Instance = Box::leak(Box::new(instance));
        registry.retain(|known| known.category != instance.category);
        // Configured instances precede prefixed ones.
        let position = registry
            .iter()
            .position(|known| known.prefixed)
            .unwrap_or(registry.len());
        registry.insert(position, instance);
    }
}

/// The registered instance with this category.
pub fn by_category(category: &str) -> Option<&'static Instance> {
    registry()
        .iter()
        .copied()
        .find(|instance| instance.category == category)
}

/// The registered instance of `base` whose root has this `host[:port]`.
pub fn by_netloc(base: Base, netloc: &str) -> Option<&'static Instance> {
    registry()
        .iter()
        .copied()
        .find(|instance| instance.base == base && instance.netloc == netloc)
}

/// The built-in family owning a URL's host, if any. Built-in instances take
/// precedence over configured patterns, as upstream lists them first.
fn builtin(url: &Url) -> Option<Base> {
    if let Some(site) = crate::booru::builtin_site_for_url(url) {
        return match site.dialect() {
            crate::booru::Dialect::Danbooru => Some(Base::Danbooru),
            crate::booru::Dialect::E621 => Some(Base::E621),
            _ => None,
        };
    }
    let host = url.host_str()?;
    (url.port().is_none() && crate::mastodon::INSTANCES.contains(&host)).then_some(Base::Mastodon)
}

/// The registered instance an http(s) URL belongs to: configured patterns
/// in configuration order, then prefixed hosts. Hosts of built-in
/// instances are never claimed.
pub fn find(url: &Url) -> Option<&'static Instance> {
    if !matches!(url.scheme(), "http" | "https") || builtin(url).is_some() {
        return None;
    }
    registry()
        .iter()
        .copied()
        .find(|instance| instance.matches(url))
}

/// The family and http(s) target of a `<basecategory>:URL` input.
pub fn prefixed(url: &Url) -> Option<(Base, Url)> {
    let base = Base::from_scheme(url.scheme())?;
    let target = crate::extract::prefixed_target(url)?;
    Some((base, target))
}

/// Resolve a `<basecategory>:URL` input to its http(s) URL, registering
/// the host as an instance of that family (category `host[:port]`, root
/// `scheme://host[:port]`) unless a built-in or configured instance of the
/// same family already serves it. Other URLs are returned unchanged.
pub fn unprefix(url: Url) -> Result<Url> {
    let Some((base, target)) = prefixed(&url) else {
        return Ok(url);
    };
    let conflict = || error("prefixed URL host belongs to another extractor family");
    if let Some(owner) = builtin(&target) {
        return if owner == base {
            Ok(target)
        } else {
            Err(conflict())
        };
    }
    let mut registry = REGISTRY
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(known) = registry.iter().find(|instance| instance.matches(&target)) {
        return if known.base == base {
            Ok(target)
        } else {
            Err(conflict())
        };
    }
    let netloc = netloc(&target).ok_or_else(conflict)?;
    if !valid_category(&netloc.replace(':', "_"))
        || reserved(&netloc)
        || registry.iter().any(|known| known.category == netloc)
    {
        return Err(error("prefixed URL host cannot be used as a category"));
    }
    let root = Url::parse(&format!("{}://{netloc}/", target.scheme())).map_err(|_| conflict())?;
    let instance = Box::leak(Box::new(Instance::new(base, &netloc, root, None, true)));
    registry.push(instance);
    Ok(target)
}

#[cfg(test)]
#[path = "base_instances_tests.rs"]
mod tests;
