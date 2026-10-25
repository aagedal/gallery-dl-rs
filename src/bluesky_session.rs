//! Native Bluesky login (`username`/`password`), upstream's `BlueskyAPI.authenticate`,
//! plus extractor options that are not typed configuration fields (`depth`).
//!
//! Sessions are created lazily with `com.atproto.server.createSession` on the
//! configured API root (`https://bsky.social` unless `api-server` is set) and
//! kept in memory only. An expired access token is refreshed once through
//! `com.atproto.server.refreshSession`, falling back to a new login. Access
//! tokens are attached only to requests on the API root's origin; DID
//! discovery, repository reads and blob downloads never carry them.
use super::{Bluesky, MAX_RESPONSE, string};
use crate::{Result, error};
use reqwest::{StatusCode, header};
use serde_json::{Map, Value};

/// Upper bound for upstream's `depth` (reply levels of a post thread).
const MAX_DEPTH: u64 = 1000;

pub(super) struct Credentials {
    identifier: String,
    password: String,
}

pub(super) struct Session {
    pub(super) access: header::HeaderValue,
    refresh: String,
}

/// Access and refresh JWTs are compact base64url segments.
fn valid_jwt(token: &str) -> bool {
    (1..=8192).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='))
}

impl Bluesky {
    /// Log in with upstream's `username` (handle, DID or email) and `password`
    /// (preferably an app password). Without an `api-server`, API calls then
    /// use `https://bsky.social`, as upstream does for logged-in sessions.
    pub fn with_credentials(
        mut self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let username = username.filter(|value| !value.is_empty());
        let password = password.filter(|value| !value.is_empty());
        self.credentials = match (username, password) {
            (None, None) => None,
            (Some(identifier), Some(password)) => Some(Credentials {
                identifier: identifier.into(),
                password: password.into(),
            }),
            _ => {
                return Err(error(
                    "native Bluesky login requires both username and password",
                ));
            }
        };
        if self.credentials.is_some() && !self.api_server_configured {
            self.api_root = url::Url::parse("https://bsky.social/xrpc/").expect("static URL");
        }
        Ok(self)
    }

    /// Apply upstream settings without typed configuration fields: `depth`.
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        for (key, value) in options.into_iter().flatten() {
            let invalid = || error(format!("invalid Bluesky setting: {key}"));
            match key.as_str() {
                "depth" => {
                    self.depth = match value {
                        Value::Null => 0,
                        Value::Number(n) => n.as_u64().ok_or_else(invalid)?,
                        Value::String(s) if !s.is_empty() && s.len() <= 4 => {
                            s.parse().map_err(|_| invalid())?
                        }
                        _ => return Err(invalid()),
                    };
                    if self.depth > MAX_DEPTH {
                        return Err(invalid());
                    }
                }
                _ => return Err(error(format!("unsupported Bluesky setting: {key}"))),
            }
        }
        Ok(self)
    }

    pub(super) fn logged_in(&self) -> bool {
        self.credentials.is_some()
    }

    /// The bearer value for API-origin requests, logging in on first use.
    pub(super) async fn access_token(
        &self,
        endpoint: &url::Url,
    ) -> Result<Option<header::HeaderValue>> {
        if self.credentials.is_none() || endpoint.origin() != self.api_root.origin() {
            return Ok(None);
        }
        let mut session = self.session.lock().await;
        if session.is_none() {
            *session = Some(self.create_session().await?);
        }
        Ok(session.as_ref().map(|session| session.access.clone()))
    }

    /// Replace an expired access token (unless another request already did).
    pub(super) async fn renew_session(&self, stale: &header::HeaderValue) -> Result<()> {
        let mut session = self.session.lock().await;
        let refresh = match session.as_ref() {
            Some(current) if current.access != *stale => return Ok(()),
            Some(current) => Some(current.refresh.clone()),
            None => None,
        };
        let renewed = match refresh {
            Some(refresh) => match self
                .session_call("com.atproto.server.refreshSession", Some(&refresh), None)
                .await
            {
                Ok(renewed) => renewed,
                Err(_) => self.create_session().await?,
            },
            None => self.create_session().await?,
        };
        *session = Some(renewed);
        Ok(())
    }

    async fn create_session(&self) -> Result<Session> {
        let credentials = self
            .credentials
            .as_ref()
            .ok_or_else(|| error("native Bluesky login requires username and password"))?;
        let body = serde_json::json!({
            "identifier": credentials.identifier,
            "password": credentials.password,
        });
        self.session_call("com.atproto.server.createSession", None, Some(body))
            .await
    }

    async fn session_call(
        &self,
        method: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> Result<Session> {
        let endpoint = self.api_root.join(method).expect("static XRPC method");
        let call = async {
            let mut request = self
                .client
                .post(endpoint)
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "application/json");
            if let Some(bearer) = bearer {
                let mut value = header::HeaderValue::from_str(&format!("Bearer {bearer}"))
                    .map_err(|_| error("invalid Bluesky refresh token"))?;
                value.set_sensitive(true);
                request = request.header(header::AUTHORIZATION, value);
            }
            if let Some(body) = &body {
                request = request
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.to_string());
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| error("Bluesky login request failed; check network access"))?;
            let status = response.status();
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| error("could not read Bluesky login response"))?
            {
                if bytes.len() + chunk.len() > MAX_RESPONSE {
                    return Err(error("Bluesky login response exceeded 16 MiB"));
                }
                bytes.extend_from_slice(&chunk);
            }
            let data: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if status != StatusCode::OK {
                // Report only a short XRPC error code, never server text that
                // could reflect the submitted identifier.
                let code = string(&data, "error").filter(|code| {
                    code.len() <= 64 && code.bytes().all(|b| b.is_ascii_alphanumeric())
                });
                return Err(error(match code {
                    Some(code) => format!("Bluesky login failed ({code})"),
                    None => format!("Bluesky login failed (HTTP {})", status.as_u16()),
                }));
            }
            let token = |key| {
                string(&data, key)
                    .filter(|token| valid_jwt(token))
                    .ok_or_else(|| error("Bluesky login returned no valid session tokens"))
            };
            let mut access =
                header::HeaderValue::from_str(&format!("Bearer {}", token("accessJwt")?))
                    .map_err(|_| error("Bluesky login returned no valid session tokens"))?;
            access.set_sensitive(true);
            Ok(Session {
                access,
                refresh: token("refreshJwt")?.to_owned(),
            })
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| error("Bluesky login timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{event::ExtractionEvent, network::NetworkOptions};
    use futures_util::StreamExt;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
        time::Duration,
    };
    use url::Url;

    const DID: &str = "did:plc:abcdefghijklmnopqrstuvwx";
    const DID2: &str = "did:plc:bcdefghijklmnopqrstuvwxy";
    const CID1: &str = "bafkreiaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CID2: &str = "bafkreibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CID3: &str = "bafkreicccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const PASSWORD: &str = "app-pass-word-1234";

    fn response(status: &str, body: &Value) -> Vec<u8> {
        let body = body.to_string();
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// Serve canned replies, recording each request's head and any body
    /// announced by Content-Length.
    fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/xrpc/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                let head_end = loop {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                };
                let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while request.len() < head_end + length {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let _ = stream.write_all(&reply);
            }
            requests
        });
        (root, task)
    }

    fn bluesky() -> Bluesky {
        Bluesky::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_blob_test_root(Some(Url::parse("https://bsky.social/").unwrap()))
    }

    fn options(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn post(did: &str, rkey: &str, cid: Option<&str>) -> Value {
        let embed = match cid {
            Some(cid) => json!({"$type": "app.bsky.embed.images", "images": [
                {"alt": "", "image": {"$type": "blob", "ref": {"$link": cid},
                 "mimeType": "image/jpeg", "size": 1}}]}),
            None => Value::Null,
        };
        json!({
            "uri": format!("at://{did}/app.bsky.feed.post/{rkey}"),
            "cid": "bafyreipost",
            "author": {"did": did, "handle": "alice.bsky.social"},
            "record": {"$type": "app.bsky.feed.post", "text": "hi",
                       "createdAt": "2024-01-02T03:04:05.678Z", "embed": embed},
        })
    }

    fn node(post: Value, replies: Vec<Value>) -> Value {
        json!({"$type": "app.bsky.feed.defs#threadViewPost", "post": post, "replies": replies})
    }

    fn session(access: &str, refresh: &str) -> Value {
        json!({"did": DID, "handle": "alice.bsky.social", "accessJwt": access,
            "refreshJwt": refresh})
    }

    fn post_ids(events: &[Result<ExtractionEvent>]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Ok(ExtractionEvent::Media(media)) => {
                    Some(media.metadata.as_ref().unwrap().post_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn depth_emits_replies_breadth_first_and_skips_unavailable_ones() {
        let thread = node(
            post(DID, "3ka", Some(CID1)),
            vec![
                node(
                    post(DID2, "3kb", Some(CID2)),
                    vec![node(post(DID, "3kd", Some(CID3)), vec![])],
                ),
                json!({"$type": "app.bsky.feed.defs#notFoundPost",
                    "uri": format!("at://{DID}/app.bsky.feed.post/3kx"), "notFound": true}),
                node(post(DID2, "3kc", None), vec![]),
            ],
        );
        let (root, server) = serve(vec![response("200 OK", &json!({"thread": thread}))]);
        let events: Vec<_> = bluesky()
            .with_api_root(root)
            .with_options(Some(&options(json!({"depth": "2"}))))
            .unwrap()
            .events(&Url::parse(&format!("https://bsky.app/profile/{DID}/post/3ka")).unwrap())
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(post_ids(&events), ["3ka", "3kb", "3kd"]);
        let requests = server.join().unwrap();
        assert!(
            requests[0].contains("depth=2&parentHeight=0"),
            "{}",
            requests[0]
        );
        assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    }

    #[tokio::test]
    async fn media_free_threads_fail_and_invalid_depths_are_rejected() {
        let thread = node(
            post(DID, "3ka", None),
            vec![node(post(DID2, "3kb", None), vec![])],
        );
        let (root, server) = serve(vec![response("200 OK", &json!({"thread": thread}))]);
        let events: Vec<_> = bluesky()
            .with_api_root(root)
            .with_options(Some(&options(json!({"depth": 1}))))
            .unwrap()
            .events(&Url::parse(&format!("https://bsky.app/profile/{DID}/post/3ka")).unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 1);
        let error = events[0].as_ref().unwrap_err().to_string();
        assert!(error.contains("no selected media"), "{error}");
        assert!(server.join().unwrap()[0].contains("depth=1&"));
        for bad in [json!(-1), json!(1001), json!("x"), json!(true), json!(1.5)] {
            let error = Bluesky::new(Duration::from_secs(5))
                .unwrap()
                .with_options(Some(&options(json!({"depth": bad}))))
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("depth"), "{error}");
        }
        assert!(
            Bluesky::new(Duration::from_secs(5))
                .unwrap()
                .with_options(Some(&options(json!({"other": 1}))))
                .is_err()
        );
    }

    #[tokio::test]
    async fn bookmarks_require_credentials_and_make_no_requests_without_them() {
        let events: Vec<_> = bluesky()
            .with_api_root(Url::parse("http://127.0.0.1:9/xrpc/").unwrap())
            .events(&Url::parse("https://bsky.app/saved").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 1);
        let error = events[0].as_ref().unwrap_err().to_string();
        assert!(error.contains("username and password"), "{error}");
        for (username, password) in [(Some("alice"), None), (None, Some(PASSWORD))] {
            let error = Bluesky::new(Duration::from_secs(5))
                .unwrap()
                .with_credentials(username, password)
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains(PASSWORD), "{error}");
        }
    }

    #[tokio::test]
    async fn login_authenticates_api_calls_and_bookmarks_page_until_empty() {
        let first = json!({"cursor": "c1", "bookmarks": [
            {"subject": {}, "item": post(DID2, "3ka", Some(CID1))},
            {"subject": {}, "item": {"$type": "app.bsky.feed.defs#notFoundPost",
                "uri": format!("at://{DID}/app.bsky.feed.post/3kx"), "notFound": true}},
            {"subject": {}, "item": post(DID, "3kb", Some(CID2))},
        ]});
        let (root, server) = serve(vec![
            response("200 OK", &session("access.one", "refresh.one")),
            response("200 OK", &first),
            response("200 OK", &json!({"cursor": "c2", "bookmarks": []})),
        ]);
        let events: Vec<_> = bluesky()
            .with_credentials(Some("alice.bsky.social"), Some(PASSWORD))
            .unwrap()
            .with_api_root(root)
            .events(&Url::parse("https://bsky.app/saved").unwrap())
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(post_ids(&events), ["3ka", "3kb"]);
        for event in &events {
            if let Ok(ExtractionEvent::Media(media)) = event {
                assert!(media.headers.is_empty() && !media.url.as_str().contains("access"));
            }
        }
        let requests = server.join().unwrap();
        let login = requests[0].to_ascii_lowercase();
        assert!(login.starts_with("post /xrpc/com.atproto.server.createsession "));
        assert!(login.contains("content-type: application/json"));
        let body: Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"identifier": "alice.bsky.social", "password": PASSWORD})
        );
        assert!(
            requests[1].starts_with("GET /xrpc/app.bsky.bookmark.getBookmarks?limit=100 "),
            "{}",
            requests[1]
        );
        assert!(
            requests[2].contains("limit=100&cursor=c1 "),
            "{}",
            requests[2]
        );
        for request in &requests[1..] {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer access.one\r\n"),
                "{request}"
            );
            assert!(!request.contains(PASSWORD));
        }
    }

    #[tokio::test]
    async fn expired_tokens_refresh_once_and_failed_logins_hide_credentials() {
        let resolved = json!({"did": DID});
        let thread = json!({"thread": node(post(DID, "3ka", Some(CID1)), vec![])});
        let (root, server) = serve(vec![
            response("200 OK", &session("access.one", "refresh.one")),
            response(
                "400 Bad Request",
                &json!({"error": "ExpiredToken", "message": "Token has expired"}),
            ),
            response("200 OK", &session("access.two", "refresh.two")),
            response("200 OK", &resolved),
            response("200 OK", &thread),
        ]);
        let events: Vec<_> = bluesky()
            .with_credentials(Some("alice.bsky.social"), Some(PASSWORD))
            .unwrap()
            .with_api_root(root)
            .events(&Url::parse("https://bsky.app/profile/alice.bsky.social/post/3ka").unwrap())
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(post_ids(&events), ["3ka"]);
        let requests: Vec<String> = server
            .join()
            .unwrap()
            .into_iter()
            .map(|request| request.to_ascii_lowercase())
            .collect();
        assert!(requests[1].starts_with("get /xrpc/com.atproto.identity.resolvehandle?"));
        assert!(requests[1].contains("authorization: bearer access.one\r\n"));
        assert!(requests[2].starts_with("post /xrpc/com.atproto.server.refreshsession "));
        assert!(requests[2].contains("authorization: bearer refresh.one\r\n"));
        assert!(requests[3].starts_with("get /xrpc/com.atproto.identity.resolvehandle?"));
        assert!(requests[3].contains("authorization: bearer access.two\r\n"));
        assert!(requests[4].contains("authorization: bearer access.two\r\n"));

        let (root, server) = serve(vec![response(
            "401 Unauthorized",
            &json!({"error": "AuthenticationRequired",
                "message": format!("Invalid identifier or password {PASSWORD}")}),
        )]);
        let events: Vec<_> = bluesky()
            .with_credentials(Some("alice.bsky.social"), Some(PASSWORD))
            .unwrap()
            .with_api_root(root)
            .events(&Url::parse("https://bsky.app/saved").unwrap())
            .collect()
            .await;
        let error = events[0].as_ref().unwrap_err().to_string();
        assert!(error.contains("AuthenticationRequired"), "{error}");
        assert!(
            !error.contains(PASSWORD) && !error.contains("alice"),
            "{error}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn liked_threads_follow_depth_and_tokens_stay_on_the_api_origin() {
        let liked = post(DID2, "3ka", Some(CID1));
        let record = json!({"uri": format!("at://{DID}/app.bsky.feed.like/3kl"), "cid": CID3,
            "value": {"$type": "app.bsky.feed.like",
                "subject": {"uri": liked["uri"], "cid": liked["cid"]}}});
        let (pds, pds_server) = serve(vec![response("200 OK", &json!({"records": [record]}))]);
        let pds_root = pds.join("../").unwrap();
        let (discovery, discovery_server) = serve(vec![response(
            "200 OK",
            &json!({"id": DID, "service": [{"id": "#atproto_pds",
                "type": "AtprotoPersonalDataServer", "serviceEndpoint": pds_root}]}),
        )]);
        let thread = node(liked, vec![node(post(DID, "3kb", Some(CID2)), vec![])]);
        let (api, api_server) = serve(vec![
            response("200 OK", &session("access.one", "refresh.one")),
            response("200 OK", &json!({"thread": thread})),
        ]);
        let events: Vec<_> = bluesky()
            .with_credentials(Some("alice.bsky.social"), Some(PASSWORD))
            .unwrap()
            .with_api_root(api)
            .with_options(Some(&options(json!({"depth": 1}))))
            .unwrap()
            .with_likes_test_roots(discovery.join("../").unwrap(), pds_root)
            .events(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap())
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(post_ids(&events), ["3ka", "3kb"]);
        for request in discovery_server
            .join()
            .unwrap()
            .into_iter()
            .chain(pds_server.join().unwrap())
        {
            assert!(
                !request.to_ascii_lowercase().contains("authorization:"),
                "{request}"
            );
        }
        let api_requests = api_server.join().unwrap();
        assert!(api_requests[1].contains("depth=1&"), "{}", api_requests[1]);
        assert!(
            api_requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer access.one\r\n")
        );
    }

    #[test]
    fn login_switches_to_the_entryway_unless_an_api_server_is_configured() {
        let native = Bluesky::new(Duration::from_secs(5))
            .unwrap()
            .with_credentials(Some("alice.bsky.social"), Some(PASSWORD))
            .unwrap();
        assert_eq!(native.api_root.as_str(), "https://bsky.social/xrpc/");
        assert!(native.logged_in());
        let native = Bluesky::new(Duration::from_secs(5))
            .unwrap()
            .with_api_server(Some("https://pds.example/base"))
            .unwrap()
            .with_credentials(Some("alice.bsky.social"), Some(PASSWORD))
            .unwrap();
        assert_eq!(native.api_root.as_str(), "https://pds.example/base/xrpc/");
        let native = Bluesky::new(Duration::from_secs(5))
            .unwrap()
            .with_credentials(None, None)
            .unwrap();
        assert_eq!(native.api_root.as_str(), super::super::API_ROOT);
        assert!(!native.logged_in());
    }
}
