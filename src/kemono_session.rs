//! Scoped, rotating API cookies. Media jobs never receive session headers.
use crate::{Result, cookies::CookieJar, error};
use reqwest::header::{COOKIE, HeaderMap, HeaderValue};
use std::{fmt, sync::Mutex};
use url::Url;

/// A Kemono/Coomer API session backed by the shared URL-scoped cookie parser.
/// Callers must validate redirects before preparing headers for the next URL.
/// No cookie snapshot is retained across requests or copied into media metadata.
#[derive(Default)]
pub struct CookieSession {
    cookies: Mutex<CookieJar>,
}

impl fmt::Debug for CookieSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CookieSession { cookies: [redacted] }")
    }
}

impl CookieSession {
    pub fn from_cookie_jar(cookies: CookieJar) -> Self {
        Self {
            cookies: Mutex::new(cookies),
        }
    }

    /// Snapshot cookies for this exact request URL, respecting domain, path,
    /// expiry and Secure attributes. The lock is released before network I/O.
    pub fn request_headers(&self, target: &Url) -> Result<HeaderMap> {
        let cookie = self
            .cookies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .header(target);
        let mut headers = HeaderMap::new();
        if let Some(cookie) = cookie {
            let mut value = HeaderValue::from_str(&cookie)
                .map_err(|_| error("invalid Kemono cookie header"))?;
            value.set_sensitive(true);
            headers.insert(COOKIE, value);
        }
        Ok(headers)
    }

    /// Apply response cookies at the URL that returned them, including redirect
    /// responses. CookieJar rejects foreign domains and malformed attributes.
    pub fn store_response(&self, source: &Url, headers: &HeaderMap) {
        self.cookies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .store_response(source, headers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Redirects;
    use reqwest::{
        StatusCode,
        header::{LOCATION, SET_COOKIE},
    };

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    fn cookies(session: &CookieSession, target: &str) -> Option<String> {
        session
            .request_headers(&url(target))
            .unwrap()
            .get(COOKIE)
            .map(|value| value.to_str().unwrap().to_owned())
    }

    #[test]
    fn imported_cookies_keep_domain_path_secure_and_expiration_scope() {
        let session = CookieSession::from_cookie_jar(
            CookieJar::parse(
                "kemono.cr\tFALSE\t/api/v1/account\tTRUE\t0\tsession\tprivate-session\n\
             .kemono.cr\tTRUE\t/api\tTRUE\t0\tshared\tprivate-shared\n\
             kemono.cr\tFALSE\t/\tFALSE\t1\texpired\tprivate-expired\n\
             coomer.st\tFALSE\t/api\tTRUE\t0\tcoomer\tprivate-coomer\n",
            )
            .unwrap(),
        );
        assert_eq!(
            cookies(&session, "https://kemono.cr/api/v1/account/favorites").as_deref(),
            Some("session=private-session; shared=private-shared")
        );
        assert_eq!(
            cookies(&session, "https://kemono.cr/api/v1/posts").as_deref(),
            Some("shared=private-shared")
        );
        assert_eq!(
            cookies(&session, "https://beta.kemono.cr/api/v1/posts").as_deref(),
            Some("shared=private-shared")
        );
        assert_eq!(
            cookies(&session, "https://coomer.st/api/v1/posts").as_deref(),
            Some("coomer=private-coomer")
        );
        for target in [
            "http://kemono.cr/api/v1/account/favorites",
            "https://kemono.cr/apiary",
            "https://kemono.cr/data/image.jpg",
            "https://kemono.su/api/v1/posts",
            "https://kemono.cr.evil.test/api/v1/posts",
            "https://example.com/api/v1/posts",
        ] {
            assert_eq!(cookies(&session, target), None, "{target}");
        }
        let headers = session
            .request_headers(&url("https://kemono.cr/api/v1/account/favorites"))
            .unwrap();
        assert!(headers.get(COOKIE).unwrap().is_sensitive());
        assert!(!format!("{session:?}").contains("private"));
        assert!(!format!("{headers:?}").contains("private"));
    }

    #[test]
    fn redirect_responses_rotate_and_delete_cookies_without_cross_origin_forwarding() {
        let session = CookieSession::default();
        let origin = url("https://kemono.cr/api/v1/account/favorites");
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("session=first; Path=/api; Secure; HttpOnly"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("foreign=private; Domain=coomer.st; Path=/"),
        );
        headers.insert(LOCATION, HeaderValue::from_static("/api/v1/account/next"));
        session.store_response(&origin, &headers);
        let mut redirects = Redirects::new(&origin);
        let next = redirects
            .next(&origin, StatusCode::FOUND, &headers)
            .unwrap()
            .unwrap();
        assert_eq!(
            cookies(&session, next.as_str()).as_deref(),
            Some("session=first")
        );

        let mut changed = HeaderMap::new();
        changed.append(
            SET_COOKIE,
            HeaderValue::from_static("session=rotated; Path=/api; Secure; HttpOnly"),
        );
        changed.append(
            SET_COOKIE,
            HeaderValue::from_static("second=yes; Path=/api; Secure"),
        );
        session.store_response(&next, &changed);
        assert_eq!(
            cookies(&session, origin.as_str()).as_deref(),
            Some("session=rotated; second=yes")
        );
        assert_eq!(
            cookies(&session, "https://coomer.st/api/v1/account/favorites"),
            None
        );

        for foreign in [
            "https://n1.kemono.cr/api/v1/account/next",
            "https://coomer.st/api/v1/account/next",
            "http://kemono.cr/api/v1/account/next",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(LOCATION, HeaderValue::from_str(foreign).unwrap());
            assert!(
                Redirects::new(&origin)
                    .next(&origin, StatusCode::FOUND, &headers)
                    .is_err()
            );
        }
        let mut deleted = HeaderMap::new();
        deleted.append(
            SET_COOKIE,
            HeaderValue::from_static("session=; Path=/api; Max-Age=0; Secure"),
        );
        session.store_response(&next, &deleted);
        assert_eq!(
            cookies(&session, origin.as_str()).as_deref(),
            Some("second=yes")
        );
    }
}
