//! Netscape cookie files with URL-scoped selection.

use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, SET_COOKIE};
use url::{Host, Url};

use crate::{Result, error};

#[derive(Debug, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

// Deliberately omit Debug: session credentials must not appear in diagnostics.
struct Cookie {
    domain: String,
    subdomains: bool,
    path: String,
    secure: bool,
    expires: i64,
    name: String,
    value: String,
}

impl std::fmt::Debug for Cookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cookie { [redacted] }")
    }
}

impl CookieJar {
    /// Parse the seven tab-separated fields of a Netscape cookie file.
    /// Errors identify the line, never its contents.
    pub fn parse(input: &str) -> Result<Self> {
        let mut cookies = Vec::new();
        for (index, line) in input.trim_start_matches('\u{feff}').lines().enumerate() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let invalid = || error(format!("invalid Netscape cookie on line {}", index + 1));
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 7 {
                return Err(invalid());
            }
            let parse_flag = |value: &str| {
                if value.eq_ignore_ascii_case("TRUE") {
                    Some(true)
                } else if value.eq_ignore_ascii_case("FALSE") {
                    Some(false)
                } else {
                    None
                }
            };
            let domain = fields[0].strip_prefix('.').unwrap_or(fields[0]);
            // Host parsing normalizes ASCII case and international domain names.
            let domain = match Host::parse(domain).map_err(|_| invalid())? {
                Host::Domain(domain) if !domain.is_empty() && !domain.ends_with('.') => domain,
                Host::Ipv4(address) => address.to_string(),
                Host::Ipv6(address) => address.to_string(),
                _ => return Err(invalid()),
            };
            let subdomains = parse_flag(fields[1]).ok_or_else(invalid)?;
            let path = fields[2];
            let secure = parse_flag(fields[3]).ok_or_else(invalid)?;
            let expires = fields[4].parse::<i64>().map_err(|_| invalid())?;
            let name = fields[5];
            let value = fields[6];
            if !path.starts_with('/')
                || path.bytes().any(|byte| byte.is_ascii_control())
                || name.is_empty()
                || !name.bytes().all(is_token)
                || !valid_cookie_value(value)
            {
                return Err(invalid());
            }
            cookies.push(Cookie {
                domain,
                subdomains,
                path: path.into(),
                secure,
                expires,
                name: name.into(),
                value: value.into(),
            });
        }
        Ok(Self { cookies })
    }

    /// Apply response cookies in memory, scoped to the URL that returned them.
    /// Invalid cookies are ignored without exposing their contents in diagnostics.
    pub fn store_response(&mut self, url: &Url, headers: &HeaderMap) {
        let now = unix_time();
        for value in headers.get_all(SET_COOKIE) {
            if let Ok(value) = value.to_str() {
                self.store_at(url, value, now);
            }
        }
    }

    fn store_at(&mut self, url: &Url, value: &str, now: i64) {
        // Bound server-controlled growth, independently of the input file size.
        const MAX_COOKIE_BYTES: usize = 4096;
        const MAX_COOKIES: usize = 256;
        if !matches!(url.scheme(), "http" | "https")
            || value.len() > MAX_COOKIE_BYTES
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
        {
            return;
        }
        // cookie 0.18 treats an empty Max-Age as infinite and a lone '-' as
        // zero. Ignore malformed attributes so a valid Expires still applies.
        let value = value
            .split(';')
            .enumerate()
            .filter(|(index, attribute)| {
                if *index == 0 {
                    return true;
                }
                let Some((name, value)) = attribute.split_once('=') else {
                    return true;
                };
                if !name.trim().eq_ignore_ascii_case("max-age") {
                    return true;
                }
                let value = value.trim();
                let digits = value.strip_prefix('-').unwrap_or(value);
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map(|(_, attribute)| attribute)
            .collect::<Vec<_>>()
            .join(";");
        let Ok(parsed) = cookie::Cookie::parse(value.as_str()) else {
            return;
        };
        let name = parsed.name();
        if name.is_empty() || !name.bytes().all(is_token) || !valid_cookie_value(parsed.value()) {
            return;
        }
        let Some(host) = url.host() else { return };
        let host_name = host.to_string();
        // Host::Ipv6's Display includes brackets; use the same canonical form as
        // Netscape imports and request selection instead.
        let host_name = host_name.trim_start_matches('[').trim_end_matches(']');
        let (domain, subdomains) = match parsed.domain() {
            None => (host_name.to_owned(), false),
            Some(domain) => {
                let Ok(Host::Domain(domain)) = Host::parse(domain) else {
                    return;
                };
                if domain.ends_with('.')
                    || !matches!(host, Host::Domain(_))
                    || !domain_matches(host_name, &domain)
                    || psl::suffix_str(&domain) == Some(domain.as_str())
                {
                    return;
                }
                (domain, true)
            }
        };
        let path = parsed
            .path()
            .filter(|path| path.starts_with('/'))
            .unwrap_or_else(|| {
                let path = url.path();
                let last_slash = path.rfind('/').unwrap_or(0);
                if last_slash == 0 {
                    "/"
                } else {
                    &path[..last_slash]
                }
            });
        if path.bytes().any(|byte| byte.is_ascii_control()) {
            return;
        }
        let secure = parsed.secure() == Some(true);
        if (secure && url.scheme() != "https")
            || (name.starts_with("__Secure-") && !secure)
            || (name.starts_with("__Host-")
                && (!secure || parsed.domain().is_some() || parsed.path() != Some("/")))
        {
            return;
        }
        // An insecure response cannot replace, delete, or shadow a secure cookie.
        if url.scheme() != "https"
            && self.cookies.iter().any(|cookie| {
                cookie.secure
                    && cookie.name == name
                    && (cookie.expires == 0 || cookie.expires > now)
                    && (domain_matches(&domain, &cookie.domain)
                        || domain_matches(&cookie.domain, &domain))
                    && path_matches(path, &cookie.path)
            })
        {
            return;
        }
        let expiry = parsed
            .max_age()
            .map(|age| {
                let seconds = age.whole_seconds();
                if seconds <= 0 {
                    now
                } else {
                    now.saturating_add(seconds)
                }
            })
            .or_else(|| parsed.expires_datetime().map(|date| date.unix_timestamp()));
        let matches =
            |cookie: &Cookie| cookie.name == name && cookie.domain == domain && cookie.path == path;
        if expiry.is_some_and(|expires| expires <= now) {
            self.cookies.retain(|cookie| !matches(cookie));
            return;
        }
        let replacement = Cookie {
            domain,
            subdomains,
            path: path.into(),
            secure,
            expires: expiry.unwrap_or(0),
            name: name.into(),
            value: parsed.value().into(),
        };
        if let Some(index) = self.cookies.iter().position(|cookie| {
            cookie.name == replacement.name
                && cookie.domain == replacement.domain
                && cookie.path == replacement.path
        }) {
            // Imported files may contain duplicate identities. Keep creation order.
            let mut position = 0;
            self.cookies.retain(|cookie| {
                let keep = position <= index
                    || cookie.name != replacement.name
                    || cookie.domain != replacement.domain
                    || cookie.path != replacement.path;
                position += 1;
                keep
            });
            self.cookies[index] = replacement;
        } else {
            self.cookies
                .retain(|cookie| cookie.expires == 0 || cookie.expires > now);
            if self.cookies.len() < MAX_COOKIES {
                self.cookies.push(replacement);
            }
        }
    }

    /// Select cookies for this URL. Do not reuse this header for other URLs.
    pub fn header(&self, url: &Url) -> Option<String> {
        self.header_at(url, unix_time())
    }

    fn header_at(&self, url: &Url, now: i64) -> Option<String> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let host = match url.host()? {
            Host::Domain(host) => host.to_owned(),
            Host::Ipv4(address) => address.to_string(),
            Host::Ipv6(address) => address.to_string(),
        };
        let path = url.path();
        let mut selected: Vec<_> = self
            .cookies
            .iter()
            .filter(|cookie| {
                let domain_matches = host == cookie.domain
                    || (cookie.subdomains
                        && matches!(url.host(), Some(Host::Domain(_)))
                        && domain_matches(&host, &cookie.domain));
                let path_matches = path_matches(path, &cookie.path);
                domain_matches
                    && path_matches
                    && (!cookie.secure || url.scheme() == "https")
                    && (cookie.expires == 0 || cookie.expires > now)
            })
            .collect();
        // Stable sorting preserves file order for paths of equal length.
        selected.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        if selected.is_empty() {
            return None;
        }
        Some(
            selected
                .into_iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn path_matches(path: &str, cookie_path: &str) -> bool {
    path == cookie_path
        || (path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn is_cookie_octet(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

fn valid_cookie_value(value: &str) -> bool {
    let contents = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    contents.bytes().all(is_cookie_octet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    #[test]
    fn response_cookies_scope_domains_paths_and_default_directories() {
        let mut jar = CookieJar::default();
        let origin = url("https://www.instagram.com/api/info/");
        jar.store_at(&origin, "host=one", 100);
        jar.store_at(
            &origin,
            "domain=two; Domain=.INSTAGRAM.COM; Path=/; Secure",
            100,
        );
        assert_eq!(
            jar.header_at(&origin, 100).as_deref(),
            Some("host=one; domain=two")
        );
        assert_eq!(
            jar.header_at(&url("https://www.instagram.com/api/info"), 100)
                .as_deref(),
            Some("host=one; domain=two")
        );
        for target in [
            "https://www.instagram.com/api/information",
            "https://m.instagram.com/api/info/",
            "https://instagram.com/",
        ] {
            assert_eq!(
                jar.header_at(&url(target), 100).as_deref(),
                Some("domain=two")
            );
        }
        for target in [
            "https://cdninstagram.com/",
            "https://instagram.com.evil.test/",
            "http://m.instagram.com/",
        ] {
            assert_eq!(jar.header_at(&url(target), 100), None);
        }
        jar.store_at(&origin, "fallback=x; Path=relative", 100);
        assert_eq!(
            jar.header_at(&origin, 100).as_deref(),
            Some("host=one; fallback=x; domain=two")
        );
    }

    #[test]
    fn rejects_foreign_public_suffix_and_malformed_response_cookies() {
        let mut jar = CookieJar::default();
        for (origin, domain) in [
            ("https://www.instagram.com/", "com"),
            ("https://www.instagram.com/", "evil.test"),
            ("https://www.instagram.com/", "instagram.com."),
            ("https://www.instagram.com/", "..instagram.com"),
            ("https://example.co.uk/", "co.uk"),
            ("https://user.github.io/", "github.io"),
            ("https://user.unknown/", "unknown"),
            ("http://127.0.0.1/", "127.0.0.1"),
        ] {
            jar.store_at(
                &url(origin),
                &format!("session=secret; Domain={domain}; Path=/"),
                100,
            );
        }
        for value in [
            "bad name=secret",
            "name=secret,other",
            "name=\"unterminated",
            "name=secret\r\ninjected=x",
            "=secret",
        ] {
            jar.store_at(&url("https://www.instagram.com/"), value, 100);
        }
        assert!(jar.cookies.is_empty());
        assert!(!format!("{jar:?}").contains("secret"));
        // IP hosts can still use host-only cookies.
        for target in ["http://127.0.0.1/", "http://[::1]/"] {
            let target = url(target);
            jar.store_at(&target, "local=x", 100);
            assert_eq!(jar.header_at(&target, 100).as_deref(), Some("local=x"));
        }
    }

    #[test]
    fn replaces_duplicates_in_order_and_deletes_only_matching_identity() {
        let mut jar = CookieJar::parse(concat!(
            ".example.com\tTRUE\t/\tTRUE\t0\tsession\told\n",
            ".example.com\tTRUE\t/\tTRUE\t0\tother\tkept\n",
            ".example.com\tTRUE\t/\tTRUE\t0\tsession\tduplicate\n",
            ".example.com\tTRUE\t/api\tTRUE\t0\tsession\tpath\n",
        ))
        .unwrap();
        let origin = url("https://www.example.com/api/info");
        jar.store_at(
            &origin,
            "session=new; Domain=example.com; Path=/; Secure",
            100,
        );
        assert_eq!(
            jar.header_at(&origin, 100).as_deref(),
            Some("session=path; session=new; other=kept")
        );
        jar.store_at(
            &origin,
            "session=; Domain=example.com; Path=/; Max-Age=0",
            100,
        );
        assert_eq!(
            jar.header_at(&origin, 100).as_deref(),
            Some("session=path; other=kept")
        );
        jar.store_at(
            &origin,
            "session=; Domain=example.com; Path=/api; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            100,
        );
        assert_eq!(jar.header_at(&origin, 100).as_deref(), Some("other=kept"));
    }

    #[test]
    fn expiry_honors_max_age_precedence_and_session_values() {
        let mut jar = CookieJar::default();
        let origin = url("https://example.com/");
        jar.store_at(
            &origin,
            "short=x; Max-Age=10; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            100,
        );
        jar.store_at(
            &origin,
            "deleted=x; Max-Age=-1; Expires=Wed, 01 Jan 2031 00:00:00 GMT",
            100,
        );
        jar.store_at(&origin, "empty=; Expires=invalid", 100);
        jar.store_at(
            &origin,
            "dated=x; Expires=Thu, 01 Jan 1970 00:03:20 GMT",
            100,
        );
        assert_eq!(
            jar.header_at(&origin, 109).as_deref(),
            Some("short=x; empty=; dated=x")
        );
        assert_eq!(
            jar.header_at(&origin, 110).as_deref(),
            Some("empty=; dated=x")
        );
        assert_eq!(jar.header_at(&origin, 200).as_deref(), Some("empty="));
        jar.store_at(&origin, "long=x; Max-Age=9223372036854775807", 100);
        assert_eq!(jar.cookies.last().unwrap().expires, i64::MAX);
        jar.store_at(
            &origin,
            "long=x; Max-Age=; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            100,
        );
        assert!(!jar.header_at(&origin, 100).unwrap().contains("long="));
        jar.store_at(
            &origin,
            "valid=x; Max-Age=-; Expires=Thu, 01 Jan 1970 00:03:20 GMT",
            100,
        );
        assert_eq!(jar.cookies.last().unwrap().expires, 200);
        jar.store_at(
            &origin,
            "valid=x; Max-Age=10; Max-Age=; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            100,
        );
        assert_eq!(jar.cookies.last().unwrap().expires, 110);
    }

    #[test]
    fn protects_secure_cookies_and_enforces_cookie_prefixes() {
        let mut jar = CookieJar::default();
        let https = url("https://www.example.com/api/info");
        let http = url("http://www.example.com/api/info");
        jar.store_at(
            &https,
            "session=secret; Domain=example.com; Path=/; Secure",
            100,
        );
        for value in [
            "session=replaced; Domain=example.com; Path=/",
            "session=; Domain=example.com; Path=/; Max-Age=0",
            "session=shadow; Path=/api",
            "insecure=x; Secure",
            "__Secure-bad=x; Path=/",
            "__Host-bad=x; Secure; Path=/",
        ] {
            jar.store_at(&http, value, 100);
        }
        for value in [
            "__Secure-bad=x",
            "__Host-bad=x; Path=/",
            "__Host-bad=x; Secure",
            "__Host-bad=x; Secure; Path=/api",
            "__Host-bad=x; Secure; Path=/; Domain=www.example.com",
        ] {
            jar.store_at(&https, value, 100);
        }
        assert_eq!(
            jar.header_at(&https, 100).as_deref(),
            Some("session=secret")
        );
        assert_eq!(jar.header_at(&http, 100), None);
        jar.store_at(&https, "__Host-good=x; Secure; Path=/", 100);
        jar.store_at(&https, "__Secure-good=y; Secure; Path=/", 100);
        assert_eq!(
            jar.header_at(&https, 100).as_deref(),
            Some("session=secret; __Host-good=x; __Secure-good=y")
        );
    }

    #[test]
    fn bounds_updates_but_allows_replacement_deletion_and_expired_slot_reuse() {
        let mut jar = CookieJar::default();
        let origin = url("https://example.com/");
        jar.store_at(&origin, &format!("oversize={}", "x".repeat(4096)), 100);
        assert!(jar.cookies.is_empty());
        for i in 0..300 {
            jar.store_at(&origin, &format!("c{i}=x; Max-Age=10"), 100);
        }
        assert_eq!(jar.cookies.len(), 256);
        jar.store_at(&origin, "c0=updated", 100);
        assert_eq!(jar.cookies[0].value, "updated");
        jar.store_at(&origin, "c1=; Max-Age=0", 100);
        jar.store_at(&origin, "fresh=x", 110);
        assert_eq!(
            jar.header_at(&origin, 110).as_deref(),
            Some("c0=updated; fresh=x")
        );
    }

    #[test]
    fn handles_each_set_cookie_header_without_splitting_expiry_dates() {
        let mut jar = CookieJar::default();
        let origin = url("https://example.com/");
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            "first=x; Expires=Wed, 01 Jan 2031 00:00:00 GMT"
                .parse()
                .unwrap(),
        );
        headers.append(
            SET_COOKIE,
            "second=y; HttpOnly; SameSite=Lax".parse().unwrap(),
        );
        headers.append(SET_COOKIE, "ignored=bad,value".parse().unwrap());
        jar.store_response(&origin, &headers);
        assert_eq!(jar.header(&origin).as_deref(), Some("first=x; second=y"));
    }

    #[test]
    fn scopes_domains_and_never_sends_instagram_cookies_to_cdn() {
        let jar = CookieJar::parse(
            "# Netscape HTTP Cookie File\n#HttpOnly_.instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\tsecret\nwww.instagram.com\tFALSE\t/\tFALSE\t0\thost\tonly\n",
        ).unwrap();
        assert_eq!(
            jar.header(&url("https://www.instagram.com/p/1")).as_deref(),
            Some("sessionid=secret; host=only")
        );
        assert_eq!(
            jar.header(&url("https://instagram.com/")).as_deref(),
            Some("sessionid=secret")
        );
        for target in [
            "https://cdninstagram.com/",
            "https://instagram.com.evil.test/",
            "https://scontent.cdninstagram.com/",
            "https://sub.www.instagram.com/",
        ] {
            let header = jar.header(&url(target));
            if target.contains("sub.www") {
                assert_eq!(header.as_deref(), Some("sessionid=secret"));
            } else {
                assert_eq!(header, None);
            }
        }
        assert_eq!(
            jar.header(&url("http://www.instagram.com/")).as_deref(),
            Some("host=only")
        );
    }

    #[test]
    fn matches_path_boundaries_and_orders_longest_first() {
        let jar = CookieJar::parse("example.com\tFALSE\t/\tFALSE\t0\ta\troot\nexample.com\tFALSE\t/foo\tFALSE\t0\ta\tchild\nexample.com\tFALSE\t/foo/\tFALSE\t0\tb\tdeep\n").unwrap();
        assert_eq!(
            jar.header(&url("https://example.com/foo/bar")).as_deref(),
            Some("b=deep; a=child; a=root")
        );
        assert_eq!(
            jar.header(&url("https://example.com/foobar")).as_deref(),
            Some("a=root")
        );
        assert_eq!(
            jar.header(&url("https://example.com/foo")).as_deref(),
            Some("a=child; a=root")
        );
    }

    #[test]
    fn respects_expiry_and_session_cookies() {
        let jar = CookieJar::parse("example.com\tFALSE\t/\tFALSE\t0\tsession\tx\nexample.com\tFALSE\t/\tFALSE\t100\texpired\tx\nexample.com\tFALSE\t/\tFALSE\t101\tfuture\tx\nexample.com\tFALSE\t/\tFALSE\t-1\tpast\tx\n").unwrap();
        assert_eq!(
            jar.header_at(&url("https://example.com/"), 100).as_deref(),
            Some("session=x; future=x")
        );
        assert_eq!(jar.header(&url("ftp://example.com/")), None);
    }

    #[test]
    fn rejects_malformed_or_unsafe_rows_without_disclosing_credentials() {
        for row in [
            "example.com\tFALSE\t/\tFALSE\t0\tname\tsecret; injected=x",
            "example.com\tFALSE\t/\tFALSE\t0\tbad name\tsecret",
            "example.com\tMAYBE\t/\tFALSE\t0\tname\tsecret",
            "example.com\tFALSE\t/\tFALSE\tinvalid\tname\tsecret",
            "example.com\tFALSE\t/\tFALSE\t0\tname\tsecret\textra",
        ] {
            let error = CookieJar::parse(row).unwrap_err().to_string();
            assert_eq!(error, "invalid Netscape cookie on line 1");
        }
    }

    #[test]
    fn supports_empty_values_crlf_and_case_normalization() {
        let jar =
            CookieJar::parse("\u{feff}# comment\r\n.EXAMPLE.COM\tTRUE\t/\tFALSE\t0\tempty\t\r\n")
                .unwrap();
        assert_eq!(
            jar.header(&url("https://sub.example.com/")).as_deref(),
            Some("empty=")
        );
        assert!(!format!("{jar:?}").contains("empty"));
        assert_eq!(
            CookieJar::parse("# comment\n")
                .unwrap()
                .header(&url("https://example.com/")),
            None
        );
    }

    #[test]
    fn accepts_rfc_quoted_values_and_ip_hosts() {
        let jar = CookieJar::parse("127.0.0.1\tTRUE\t/\tFALSE\t0\tquoted\t\"value\"\n").unwrap();
        assert_eq!(
            jar.header(&url("http://127.0.0.1/")).as_deref(),
            Some("quoted=\"value\"")
        );
        assert_eq!(jar.header(&url("http://127.0.0.2/")), None);
        assert!(CookieJar::parse("example.com\tFALSE\t/\tFALSE\t0\tname\t\"unterminated").is_err());
    }
}
