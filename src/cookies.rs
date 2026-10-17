//! Netscape cookie files with URL-scoped selection.

use std::time::{SystemTime, UNIX_EPOCH};

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

    /// Select cookies for this URL. Do not reuse this header for other URLs.
    pub fn header(&self, url: &Url) -> Option<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .min(i64::MAX as u64) as i64;
        self.header_at(url, now)
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
                        && host
                            .strip_suffix(&cookie.domain)
                            .is_some_and(|prefix| prefix.ends_with('.')));
                let path_matches = path == cookie.path
                    || (path.starts_with(&cookie.path)
                        && (cookie.path.ends_with('/')
                            || path.as_bytes().get(cookie.path.len()) == Some(&b'/')));
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
