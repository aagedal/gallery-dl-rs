//! Public Bluesky liked-post record traversal and DID service discovery.
use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LikesEndpoint {
    #[default]
    Records,
    ActorLikes,
}

fn base_root(raw: &str, allow_http: bool) -> Option<Url> {
    if raw.len() > 4096 || raw != raw.trim() || raw.chars().any(char::is_control) {
        return None;
    }
    let mut root = Url::parse(raw).ok()?;
    if !(root.scheme() == "https" || allow_http && root.scheme() == "http")
        || root.host_str().is_none()
        || !root.username().is_empty()
        || root.password().is_some()
        || root.query().is_some()
        || root.fragment().is_some()
    {
        return None;
    }
    if !root.path().ends_with('/') {
        root.set_path(&format!("{}/", root.path()));
    }
    Some(root)
}

fn service_endpoint(document: &[u8], did: &str, allow_http: bool) -> Option<Url> {
    let document: Value = serde_json::from_slice(document).ok()?;
    if document.get("id").and_then(Value::as_str) != Some(did) {
        return None;
    }
    document
        .get("service")?
        .as_array()?
        .iter()
        .find_map(|service| {
            if service.get("type").and_then(Value::as_str) != Some("AtprotoPersonalDataServer") {
                return None;
            }
            base_root(service.get("serviceEndpoint")?.as_str()?, allow_http)
        })
}

fn subject(record: &Value, owner: &str) -> Option<(String, String)> {
    let uri = record.get("uri")?.as_str()?;
    let key = uri.strip_prefix(&format!("at://{owner}/app.bsky.feed.like/"))?;
    if !valid_rkey(key) {
        return None;
    }
    if let Some(cid) = record.get("cid")
        && !cid.as_str().is_some_and(valid_cid)
    {
        return None;
    }
    let value = record.get("value")?;
    if value.get("$type").and_then(Value::as_str) != Some("app.bsky.feed.like") {
        return None;
    }
    let reference = value.get("subject")?;
    let uri = reference.get("uri")?.as_str()?;
    post_identity(uri).ok()?;
    let cid = reference.get("cid")?.as_str()?;
    valid_cid(cid).then(|| (uri.into(), cid.into()))
}

struct State {
    actor: String,
    did: Option<String>,
    service: Option<Url>,
    cursor: Option<String>,
    cursors: BTreeSet<String>,
    pages: usize,
    done: bool,
    records: VecDeque<Value>,
    active: Option<SelectedState>,
}

impl Bluesky {
    pub fn with_likes_endpoint(mut self, endpoint: LikesEndpoint) -> Self {
        self.likes_endpoint = endpoint;
        self
    }

    /// Override only the AppView/API server. Repository reads still use the
    /// owner's DID service document. Empty explicitly restores the public API.
    pub fn with_api_server(mut self, server: Option<&str>) -> Result<Self> {
        if let Some(server) = server {
            self.api_root = if server.is_empty() {
                Url::parse(API_ROOT).unwrap()
            } else {
                base_root(server, true)
                    .and_then(|root| root.join("xrpc/").ok())
                    .ok_or_else(|| error("invalid Bluesky api-server; expected an HTTP(S) base URL without credentials, query or fragment"))?
            };
        }
        Ok(self)
    }

    #[cfg(test)]
    pub(super) fn with_likes_test_roots(mut self, discovery: Url, fallback: Url) -> Self {
        self.discovery_root = Some(discovery);
        self.pds_fallback = fallback;
        self
    }

    fn discovery_url(&self, did: &str) -> Url {
        let path = if did.starts_with("did:web:") {
            ".well-known/did.json"
        } else {
            did
        };
        if let Some(root) = &self.discovery_root {
            return root
                .join(&format!("./{path}"))
                .expect("validated DID discovery path");
        }
        if let Some(host) = did.strip_prefix("did:web:") {
            Url::parse(&format!("https://{host}/.well-known/did.json"))
                .expect("validated DID web host")
        } else {
            Url::parse(&format!("https://plc.directory/{did}")).expect("validated PLC DID")
        }
    }

    async fn personal_server(&self, did: &str) -> Url {
        let document =
            tokio::time::timeout(self.timeout, self.request(&self.discovery_url(did))).await;
        if let Ok(Ok(document)) = document
            && let Some(root) = service_endpoint(&document, did, self.discovery_root.is_some())
        {
            return root;
        }
        self.pds_fallback.clone()
    }

    async fn like_page(&self, state: &mut State) -> Result<()> {
        let did = match &state.did {
            Some(did) => did.clone(),
            None => {
                let did = self.resolve(&state.actor).await?;
                state.did = Some(did.clone());
                did
            }
        };
        let root = match &state.service {
            Some(root) => root.clone(),
            None => {
                let root = self.personal_server(&did).await;
                state.service = Some(root.clone());
                root
            }
        };
        let mut endpoint = root
            .join("xrpc/com.atproto.repo.listRecords")
            .map_err(|_| error("Bluesky DID service has an invalid base URL"))?;
        endpoint.query_pairs_mut().extend_pairs([
            ("repo", did.as_str()),
            ("collection", "app.bsky.feed.like"),
            ("limit", PAGE_LIMIT),
        ]);
        if let Some(cursor) = &state.cursor {
            endpoint.query_pairs_mut().append_pair("cursor", cursor);
        }
        let bytes = tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Bluesky repository extraction timed out"))??;
        let mut body: Value =
            serde_json::from_slice(&bytes).map_err(|_| error("Bluesky returned invalid JSON"))?;
        let cursor = page_cursor(&body)?;
        let records = body
            .get_mut("records")
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
            .ok_or_else(|| error("Bluesky response is missing its records"))?;
        state.pages += 1;
        state.done = cursor
            .as_ref()
            .is_none_or(|cursor| !state.cursors.insert(cursor.clone()))
            || state.pages >= MAX_PAGES;
        state.cursor = cursor;
        state.records.extend(records);
        Ok(())
    }

    pub(super) fn likes_events(&self, actor: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        if self.likes_endpoint == LikesEndpoint::ActorLikes {
            return self.feed_events(actor, Feed::ActorLikes);
        }
        let state = State {
            actor,
            did: None,
            service: None,
            cursor: None,
            cursors: BTreeSet::new(),
            pages: 0,
            done: false,
            records: VecDeque::new(),
            active: None,
        };
        self.record_likes_events(state)
    }

    fn record_likes_events(&self, state: State) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            let mut skipped = 0u8;
            loop {
                skipped = skipped.wrapping_add(1);
                if skipped == 0 {
                    tokio::task::yield_now().await;
                }
                if let Some(active) = &mut state.active {
                    match self.next_selected(active).await {
                        Ok(Some(event)) => return Some((Ok(event), Some(state))),
                        // Upstream skips failed liked posts and continues the public
                        // record listing. Any already-emitted group is complete.
                        Ok(None) | Err(_) => state.active = None,
                    }
                }
                if let Some(record) = state.records.pop_front() {
                    let Some((uri, cid)) =
                        subject(&record, state.did.as_deref().expect("resolved owner"))
                    else {
                        continue;
                    };
                    let body = match self
                        .call(
                            "app.bsky.feed.getPostThread",
                            &[("uri", &uri), ("depth", "0"), ("parentHeight", "0")],
                        )
                        .await
                    {
                        Ok(body) => body,
                        Err(_) => continue,
                    };
                    let post = match thread_post(&body, &uri) {
                        Ok(post)
                            if post.get("cid").and_then(Value::as_str) == Some(cid.as_str()) =>
                        {
                            post
                        }
                        _ => continue,
                    };
                    state.active = Some(SelectedState::new(post));
                    continue;
                }
                if state.done {
                    return None;
                }
                if let Err(error) = self.like_page(&mut state).await {
                    return Some((Err(error), None));
                }
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovery_requires_matching_did_and_safe_https_service_and_builds_expected_urls() {
        let owner = "did:web:owner.example";
        let document = |endpoint: &str| {
            json!({"id":owner,"service":[{"type":"AtprotoPersonalDataServer","serviceEndpoint":endpoint}]}).to_string()
        };
        assert_eq!(
            service_endpoint(
                document("https://pds.example:8443/base").as_bytes(),
                owner,
                false
            )
            .unwrap()
            .as_str(),
            "https://pds.example:8443/base/"
        );
        for endpoint in [
            "http://pds.example/",
            "https://user:secret@pds.example/",
            "https://pds.example/?token=secret",
            "https://pds.example/#private",
            "file:///private",
        ] {
            assert!(service_endpoint(document(endpoint).as_bytes(), owner, false).is_none());
        }
        let client = Bluesky::new(Duration::from_secs(1)).unwrap();
        assert_eq!(
            client.discovery_url(owner).as_str(),
            "https://owner.example/.well-known/did.json"
        );
        let did = "did:plc:abcdefghijklmnopqrstuvwx";
        assert_eq!(
            client.discovery_url(did).as_str(),
            format!("https://plc.directory/{did}")
        );
    }

    #[test]
    fn ready_malformed_record_scan_yields_and_can_be_cancelled_before_later_hydration() {
        use std::{
            future::Future,
            net::TcpListener,
            task::{Context, Poll},
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let api = Url::parse(&format!("http://{}/xrpc/", listener.local_addr().unwrap())).unwrap();
        let client = Bluesky::new(Duration::from_secs(1))
            .unwrap()
            .with_api_root(api);
        let owner = "did:web:owner.example";
        let mut records = VecDeque::from(vec![Value::Null; 1024]);
        records.push_back(json!({"uri":format!("at://{owner}/app.bsky.feed.like/a"),"value":{"$type":"app.bsky.feed.like",
            "subject":{"uri":"at://did:web:poster.example/app.bsky.feed.post/later","cid":"bafyreipost"}}}));
        let state = State {
            actor: owner.into(),
            did: Some(owner.into()),
            service: None,
            cursor: None,
            cursors: BTreeSet::new(),
            pages: 1,
            done: true,
            records,
            active: None,
        };
        let mut events = client.record_likes_events(state);
        let mut next = Box::pin(events.next());
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(matches!(next.as_mut().poll(&mut context), Poll::Pending));
        drop(next);
        drop(events);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn api_server_overrides_only_appview_and_rejects_unsafe_values_privately() {
        let client = Bluesky::new(Duration::from_secs(1))
            .unwrap()
            .with_api_server(Some("http://127.0.0.1:1234/base"))
            .unwrap();
        assert_eq!(client.api_root.as_str(), "http://127.0.0.1:1234/base/xrpc/");
        assert_eq!(client.pds_fallback.as_str(), "https://bsky.social/");
        assert_eq!(
            client.discovery_url("did:web:owner.example").as_str(),
            "https://owner.example/.well-known/did.json"
        );
        assert_eq!(
            client.with_api_server(Some("")).unwrap().api_root.as_str(),
            API_ROOT
        );
        for raw in [
            "https://user:secret@example.org/",
            "https://example.org/?secret=token",
            "https://example.org/#secret",
            "file:///secret",
            " https://example.org/",
            "https://example.org/\nsecret",
        ] {
            let error = Bluesky::new(Duration::from_secs(1))
                .unwrap()
                .with_api_server(Some(raw))
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("secret"));
        }
    }
}
