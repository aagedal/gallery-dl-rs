//! Upstream `tags-blacklist`/`tags-whitelist` post predicates, including
//! Danbooru-style rules: `a b` (all), `-a` (not), `~a ~b` (any of).
use std::collections::HashSet;

#[derive(Clone, Debug)]
enum Check {
    In(String),
    NotIn(String),
}

#[derive(Clone, Debug)]
pub struct TagFilter {
    simple: HashSet<String>,
    complex: Vec<Vec<Check>>,
    whitelist: bool,
}

impl TagFilter {
    /// Parse a comma-separated list, or the lines of the file it names.
    pub fn parse(list: &str, whitelist: bool) -> Self {
        let path = crate::postprocess::expand_home(list);
        let lines: Vec<String> = match std::fs::read_to_string(&path) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(_) => list.split(',').map(str::to_owned).collect(),
        };
        let mut simple = HashSet::new();
        let mut complex = Vec::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() || line.starts_with("# ") {
                continue;
            }
            let line = line.to_lowercase();
            if line.contains(' ') {
                let mut any = Vec::new();
                let mut all = Vec::new();
                for tag in line.split_whitespace() {
                    if let Some(tag) = tag.strip_prefix('~') {
                        any.push(tag.to_owned());
                    } else if let Some(tag) = tag.strip_prefix('-') {
                        all.push(Check::NotIn(tag.to_owned()));
                    } else {
                        all.push(Check::In(tag.to_owned()));
                    }
                }
                if any.is_empty() {
                    complex.push(all);
                } else {
                    for tag in any {
                        let mut chain = all.clone();
                        chain.push(Check::In(tag));
                        complex.push(chain);
                    }
                }
            } else if let Some(tag) = line.strip_prefix('~') {
                simple.insert(tag.to_owned());
            } else if let Some(tag) = line.strip_prefix('-') {
                complex.push(vec![Check::NotIn(tag.to_owned())]);
            } else {
                simple.insert(line);
            }
        }
        Self {
            simple,
            complex,
            whitelist,
        }
    }

    /// Whether a post with these tags is kept. Posts without tags are kept.
    pub fn allows(&self, tags: Option<&[String]>) -> bool {
        let Some(tags) = tags else {
            return true;
        };
        let matched = tags.iter().any(|tag| self.simple.contains(tag))
            || self.complex.iter().any(|chain| {
                chain.iter().all(|check| match check {
                    Check::In(tag) => tags.contains(tag),
                    Check::NotIn(tag) => !tags.contains(tag),
                })
            });
        matched == self.whitelist
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|tag| (*tag).to_owned()).collect()
    }

    #[test]
    fn danbooru_style_rules() {
        let filter = TagFilter::parse("1girl,shirt highres -blush,~a ~b", false);
        assert!(!filter.allows(Some(&tags(&["1girl"]))));
        assert!(!filter.allows(Some(&tags(&["shirt", "highres"]))));
        assert!(filter.allows(Some(&tags(&["shirt", "highres", "blush"]))));
        assert!(!filter.allows(Some(&tags(&["b"]))));
        assert!(filter.allows(Some(&tags(&["other"]))));
        assert!(filter.allows(None));
        let white = TagFilter::parse("cat", true);
        assert!(white.allows(Some(&tags(&["cat", "dog"]))));
        assert!(!white.allows(Some(&tags(&["dog"]))));
        let not = TagFilter::parse("-cat", false);
        assert!(!not.allows(Some(&tags(&["dog"]))));
        assert!(not.allows(Some(&tags(&["cat"]))));
    }
}
