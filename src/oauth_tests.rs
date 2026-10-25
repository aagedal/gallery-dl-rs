use super::*;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener as StdListener,
    thread::{self, JoinHandle},
};

fn reply(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn json_reply(body: Value) -> Vec<u8> {
    reply("200 OK", "application/json", &body.to_string())
}

/// Serve canned replies in order, recording each request with its body.
fn serve(replies: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
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

/// One GET to the local listener, returning the whole response.
fn get(address: SocketAddr, target: &str) -> String {
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: localhost:6414\r\nUser-Agent: test-browser\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

type Redirect = Arc<dyn Fn(&BTreeMap<String, String>) -> String + Send + Sync>;

/// A simulated browser: it records the authorization URL, asks for the
/// favicon first (which must be ignored), then follows the redirect whose
/// query `redirect` builds from the authorization URL's parameters.
fn browser(
    redirect: Redirect,
    seen: Arc<Mutex<Option<(Url, String)>>>,
) -> impl Fn(&Url, SocketAddr) -> bool + Send + Sync + 'static {
    move |url, address| {
        let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
        let query = redirect(&params);
        let url = url.clone();
        let seen = seen.clone();
        thread::spawn(move || {
            let favicon = get(address, "/favicon.ico");
            assert!(favicon.starts_with("HTTP/1.1 404 "), "{favicon}");
            let response = get(address, &format!("/?{query}"));
            *seen.lock().unwrap() = Some((url, response));
        });
        true
    }
}

/// The browser's view once its redirect request has been answered.
fn wait(seen: &Arc<Mutex<Option<(Url, String)>>>) -> (Url, String) {
    for _ in 0..500 {
        if let Some(seen) = seen.lock().unwrap().take() {
            return seen;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("the simulated browser received no response");
}

fn settings(values: &[(&str, &str, Value)]) -> Settings {
    Settings {
        cache: true,
        browser: true,
        port: 0,
        values: values
            .iter()
            .map(|(category, key, value)| (((*category).into(), (*key).into()), value.clone()))
            .collect(),
    }
}

fn flow(settings: Settings, cache: Option<Cache>, root: &Url) -> (Flow, Arc<Mutex<Vec<u8>>>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let flow = Flow::new(
        settings,
        cache,
        &NetworkOptions::new(Some(String::new()), None).unwrap(),
        Duration::from_secs(5),
    )
    .unwrap()
    .with_root(root.clone())
    .with_output(output.clone());
    (flow, output)
}

fn text(output: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(output.lock().unwrap().clone()).unwrap()
}

fn temp_cache() -> (tempfile::TempDir, Cache) {
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path().join("cache.sqlite3"));
    (directory, cache)
}

fn header<'r>(request: &'r str, name: &str) -> Option<&'r str> {
    request.lines().find_map(|line| {
        let (key, value) = line.split_once(": ")?;
        key.eq_ignore_ascii_case(name).then_some(value)
    })
}

fn body(request: &str) -> BTreeMap<String, String> {
    let body = request.split_once("\r\n\r\n").unwrap().1;
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

fn basic(id: &str, secret: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"))
    )
}

#[test]
fn hmac_and_signature_match_reference_vectors() {
    // RFC 2202 test cases 1 and 6 (key longer than one block).
    let hex = |bytes: [u8; 20]| bytes.map(|b| format!("{b:02x}")).concat();
    assert_eq!(
        hex(hmac_sha1(&[0x0b; 20], b"Hi There")),
        "b617318655057264e28bc0b6fb378c8ef146be00"
    );
    assert_eq!(
        hex(hmac_sha1(
            &[0xaa; 80],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "aa4ae5e15272d00e95705637ce8a3b55ed402112"
    );
    // RFC 5849 section 1.2 (with its errata-corrected signature).
    let url =
        Url::parse("http://photos.example.net/photos?file=vacation.jpg&size=original").unwrap();
    let params = [
        ("oauth_consumer_key", "dpf43f3p2l4k3l03".to_owned()),
        ("oauth_token", "nnch734d00sl2jdk".to_owned()),
        ("oauth_signature_method", "HMAC-SHA1".to_owned()),
        ("oauth_timestamp", "137131202".to_owned()),
        ("oauth_nonce", "chapoH".to_owned()),
    ];
    assert_eq!(
        signature("GET", &url, &params, "kd94hf93k423kf44", "pfkkdhi9sl3r4s00"),
        "MdpQcU8iPSUjWoN/UDMsK2sui9I="
    );
    // RFC 5849 section 1.2's temporary credential request (POST, no token).
    let url = Url::parse("https://photos.example.net/initiate").unwrap();
    let signer = OAuth1 {
        consumer_key: "dpf43f3p2l4k3l03",
        consumer_secret: "kd94hf93k423kf44",
        token: None,
        token_secret: "",
    };
    let header = signer.authorization("POST", &url, "wIjqoS", 137131200);
    assert!(header.starts_with("OAuth oauth_consumer_key=\"dpf43f3p2l4k3l03\","));
    assert!(!header.contains("oauth_token="));
    assert_eq!(quote("a b/~+="), "a%20b%2F~%2B%3D");
}

#[test]
fn nonces_tokens_and_pkce_challenges() {
    let first = nonce(16);
    assert_eq!(first.len(), 16);
    assert!(first.bytes().all(|b| b.is_ascii_alphabetic()));
    assert_ne!(first, nonce(16));
    assert_eq!(nonce(200).len(), 200);
    let token = generate_token(32).unwrap();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    // RFC 7636 appendix B.
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn targets_follow_upstream_patterns() {
    assert_eq!(target("deviantart").unwrap(), Target::Deviantart);
    assert_eq!(target("pixiv").unwrap(), Target::Pixiv);
    for input in [
        "mastodon:mastodon.social",
        "mastodon:https://Mastodon.Social/@user",
        "mastodon:http://mastodon.social?x",
    ] {
        assert_eq!(
            target(input).unwrap(),
            Target::Mastodon("mastodon.social".into()),
            "{input}"
        );
    }
    for input in [
        "deviantart/",
        "Pixiv",
        "twitter",
        "mastodon",
        "mastodon:",
        "mastodon:https://",
        "mastodon:bad host",
        "mastodon:a@b",
    ] {
        assert!(target(input).is_err(), "{input}");
    }
    assert_eq!(
        crate::extract::input_url("oauth:mastodon:pawoo.net")
            .unwrap()
            .path(),
        "mastodon:pawoo.net"
    );
    assert!(crate::extract::input_url("oauth:unknown").is_err());
}

#[test]
fn messages_match_upstream() {
    let (flow, _) = flow(Settings::default(), None, &Url::parse("http://x/").unwrap());
    assert_eq!(
        flow.message("deviantart", &["refresh-token"], &["RT"], true),
        "\nYour 'refresh-token' is\n\nRT\n\nThis value has been cached and will automatically be used.\n"
    );
    assert_eq!(
        flow.message("deviantart", &["refresh-token"], &["RT"], false),
        "\nYour 'refresh-token' is\n\nRT\n\nPut this value into your configuration file as \n'extractor.deviantart.refresh-token'\nto use it.\n"
    );
    let configured = settings(&[("tumblr", "access-token", json!("old"))]);
    let (flow, _) = self::flow(configured, None, &Url::parse("http://x/").unwrap());
    assert_eq!(
        flow.message(
            "tumblr",
            &["access-token", "access-token-secret"],
            &["T", "S"],
            true
        ),
        "\nYour 'access-token' and 'access-token-secret' are\n\nT\nS\n\nPut these values into your configuration file as \n'extractor.tumblr.access-token' and\n'extractor.tumblr.access-token-secret'\nor set\n'extractor.tumblr.access-token' to \"cache\"\nto use them.\n"
    );
}

#[tokio::test]
async fn deviantart_authorization_code_is_exchanged_and_cached() {
    let (root, server) = serve(vec![json_reply(
        json!({"access_token": "AT", "refresh_token": "RT-1", "token_type": "Bearer"}),
    )]);
    let (_directory, cache) = temp_cache();
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect = Arc::new(|params| format!("code=CODE&state={}", params["state"]));
    let (flow, output) = flow(settings(&[]), Some(cache.clone()), &root);
    flow.with_browser(browser(redirect, seen.clone()))
        .run("deviantart")
        .await
        .unwrap();

    let (url, response) = wait(&seen);
    assert_eq!(url.path(), "/oauth2/authorize");
    let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "5388");
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["redirect_uri"], REDIRECT_URI_HTTPS);
    assert_eq!(params["duration"], "permanent");
    assert_eq!(params["scope"], "browse user.manage");
    assert!(params["state"].starts_with("gallery-dl_deviantart_"));
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("been cached and will automatically be used.\n"));

    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("POST /oauth2/token HTTP/1.1\r\n"));
    assert_eq!(
        header(&requests[0], "authorization"),
        Some(basic("5388", crate::deviantart::CLIENT_SECRET).as_str())
    );
    let form = body(&requests[0]);
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "CODE");
    assert_eq!(form["redirect_uri"], REDIRECT_URI_HTTPS);

    let key = "gallery_dl.extractor.deviantart._refresh_token_cache-#5388";
    assert_eq!(cache.get_str(key).await.unwrap().as_deref(), Some("RT-1"));
    let printed = text(&output);
    assert!(printed.contains("Waiting for response. (Cancel with Ctrl+c)\n"));
    assert!(printed.contains("\nYour 'refresh-token' is\n\nRT-1\n\n"));
}

#[tokio::test]
async fn state_mismatch_is_rejected_without_a_token_request() {
    let (root, server) = serve(Vec::new());
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect = Arc::new(|_| "code=CODE&state=forged".to_owned());
    let (flow, output) = flow(settings(&[]), None, &root);
    let error = flow
        .with_browser(browser(redirect, seen.clone()))
        .run("reddit")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "OAuth response 'state' mismatch");
    let (_, response) = wait(&seen);
    assert!(response.contains("'state' mismatch: expected gallery-dl_reddit_"));
    assert!(response.ends_with(", got forged.\n"));
    assert!(text(&output).contains("'state' mismatch"));
    assert!(server.join().unwrap().is_empty());
}

#[tokio::test]
async fn reddit_errors_are_reported_and_custom_clients_use_their_cache_key() {
    // An authorization error from the redirect.
    let (root, server) = serve(Vec::new());
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect =
        Arc::new(|params| format!("error=access_denied&state={}", params["state"]));
    let (flow, _) = flow(settings(&[]), None, &root);
    assert!(
        flow.with_browser(browser(redirect, seen.clone()))
            .run("reddit")
            .await
            .is_err()
    );
    let (_, response) = wait(&seen);
    assert!(response.ends_with("Remote server reported an error:\n\naccess_denied\n"));
    assert!(server.join().unwrap().is_empty());

    // A custom client ID without a secret, cached under `#ID`.
    let (root, server) = serve(vec![json_reply(json!({"refresh_token": "RRT"}))]);
    let (_directory, cache) = temp_cache();
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect = Arc::new(|params| format!("state={}&code=C2", params["state"]));
    let configured = settings(&[("reddit", "client-id", json!("myapp"))]);
    let (flow, _) = self::flow(configured, Some(cache.clone()), &root);
    flow.with_browser(browser(redirect, seen.clone()))
        .run("reddit")
        .await
        .unwrap();
    let (url, _) = wait(&seen);
    assert_eq!(url.path(), "/api/v1/authorize");
    assert!(url.as_str().contains("scope=read+history"));
    assert!(
        url.as_str()
            .contains("redirect_uri=http%3A%2F%2Flocalhost%3A6414%2F")
    );
    let request = &server.join().unwrap()[0];
    assert!(request.starts_with("POST /api/v1/access_token HTTP/1.1\r\n"));
    assert_eq!(
        header(request, "authorization"),
        Some(basic("myapp", "").as_str())
    );
    assert_eq!(
        header(request, "user-agent"),
        Some(crate::reddit::USER_AGENT)
    );
    assert_eq!(
        cache
            .get_str("gallery_dl.extractor.reddit._refresh_token_cache-#myapp")
            .await
            .unwrap()
            .as_deref(),
        Some("RRT")
    );
}

#[tokio::test]
async fn token_endpoint_errors_are_not_cached() {
    let (root, server) = serve(vec![reply(
        "400 Bad Request",
        "application/json",
        &json!({"error": "invalid_grant", "error_description": "bad\ncode"}).to_string(),
    )]);
    let (_directory, cache) = temp_cache();
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect = Arc::new(|params| format!("code=CODE&state={}", params["state"]));
    let (flow, _) = flow(settings(&[]), Some(cache.clone()), &root);
    let error = flow
        .with_browser(browser(redirect, seen.clone()))
        .run("deviantart")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "OAuth token request failed");
    let (_, response) = wait(&seen);
    assert!(
        response.ends_with("invalid_grant (badcode)\n"),
        "{response}"
    );
    server.join().unwrap();
    assert_eq!(
        cache
            .get("gallery_dl.extractor.deviantart._refresh_token_cache-#5388")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn mastodon_uses_builtin_applications_or_registers_one() {
    // Built-in application for pawoo.net.
    let (root, server) = serve(vec![json_reply(json!({"access_token": "MAT"}))]);
    let (_directory, cache) = temp_cache();
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect = Arc::new(|params| format!("code=MC&state={}", params["state"]));
    let (flow, output) = flow(settings(&[]), Some(cache.clone()), &root);
    flow.with_browser(browser(redirect.clone(), seen.clone()))
        .run("mastodon:pawoo.net")
        .await
        .unwrap();
    let (id, secret) = crate::mastodon::application("pawoo.net").unwrap();
    let (url, _) = wait(&seen);
    assert_eq!(url.path(), "/oauth/authorize");
    assert!(url.as_str().contains(&format!("client_id={id}")));
    assert!(url.as_str().contains("scope=read&") || url.as_str().ends_with("scope=read"));
    let request = &server.join().unwrap()[0];
    assert!(request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
    assert_eq!(
        header(request, "authorization"),
        Some(basic(id, secret).as_str())
    );
    assert_eq!(
        cache
            .get_str("gallery_dl.extractor.mastodon._access_token_cache-pawoo.net")
            .await
            .unwrap()
            .as_deref(),
        Some("MAT")
    );
    assert!(text(&output).contains("\nYour 'access-token' is\n\nMAT\n\n"));

    // Other instances register an application once; it is cached.
    let (root, server) = serve(vec![
        json_reply(
            json!({"id": "1", "client_id": "CID", "client_secret": "CSECRET",
                          "redirect_uri": REDIRECT_URI_LOCALHOST, "vapid_key": null}),
        ),
        json_reply(json!({"access_token": "XAT"})),
        json_reply(json!({"access_token": "XAT2"})),
    ]);
    let (flow, output) = self::flow(settings(&[]), Some(cache.clone()), &root);
    let flow = flow.with_browser(browser(redirect.clone(), seen.clone()));
    flow.run("mastodon:example.social").await.unwrap();
    flow.run("mastodon:https://example.social/").await.unwrap();
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("POST /api/v1/apps HTTP/1.1\r\n"));
    let form = body(&requests[0]);
    assert!(form["client_name"].starts_with("gdl:"));
    assert_eq!(form["redirect_uris"], REDIRECT_URI_LOCALHOST);
    assert_eq!(form["scopes"], "read");
    for request in &requests[1..] {
        assert!(request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
        assert_eq!(
            header(request, "authorization"),
            Some(basic("CID", "CSECRET").as_str())
        );
    }
    assert_eq!(
        cache
            .get("gallery_dl.extractor.oauth._register-example.social")
            .await
            .unwrap(),
        Some(json!({"client-id": "CID", "client-secret": "CSECRET"}))
    );
    assert_eq!(
        cache
            .get_str("gallery_dl.extractor.mastodon._access_token_cache-example.social")
            .await
            .unwrap()
            .as_deref(),
        Some("XAT2")
    );
    // The client secret is never printed.
    assert!(!text(&output).contains("CSECRET"));
}

/// Verify an OAuth 1.0a request's signature against the expected secrets.
fn verify_oauth1(request: &str, root: &Url, consumer_secret: &str, token_secret: &str) {
    let line = request.lines().next().unwrap();
    let target = line.split(' ').nth(1).unwrap();
    let url = root.join(target).unwrap();
    let authorization = header(request, "authorization").unwrap();
    let fields: BTreeMap<String, String> = authorization
        .strip_prefix("OAuth ")
        .unwrap()
        .split(',')
        .map(|field| {
            let (key, value) = field.split_once('=').unwrap();
            let value = value.trim_matches('"');
            (
                key.to_owned(),
                url::form_urlencoded::parse(format!("v={}", value.replace('+', "%2B")).as_bytes())
                    .next()
                    .unwrap()
                    .1
                    .into_owned(),
            )
        })
        .collect();
    let params: Vec<(&str, String)> = fields
        .iter()
        .filter(|(key, _)| *key != "oauth_signature")
        .map(|(key, value)| (key.as_str(), value.clone()))
        .collect();
    assert_eq!(
        fields["oauth_signature"],
        signature("GET", &url, &params, consumer_secret, token_secret)
    );
}

#[tokio::test]
async fn tumblr_oauth1_flow_signs_requests_and_caches_the_token_pair() {
    let (root, server) = serve(vec![
        reply(
            "200 OK",
            "application/x-www-form-urlencoded",
            "oauth_token=REQ&oauth_token_secret=REQSECRET&oauth_callback_confirmed=true",
        ),
        reply(
            "200 OK",
            "application/x-www-form-urlencoded",
            "oauth_token=ACC&oauth_token_secret=ACCSECRET",
        ),
    ]);
    let (_directory, cache) = temp_cache();
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect =
        Arc::new(|params| format!("oauth_token={}&oauth_verifier=VER", params["oauth_token"]));
    let (flow, output) = flow(settings(&[]), Some(cache.clone()), &root);
    flow.with_browser(browser(redirect, seen.clone()))
        .run("tumblr")
        .await
        .unwrap();
    let (url, response) = wait(&seen);
    assert_eq!(url.path(), "/oauth/authorize");
    assert_eq!(url.query(), Some("oauth_token=REQ&perms=read"));
    assert!(response.contains("\nACC\nACCSECRET\n"));

    let requests = server.join().unwrap();
    assert!(requests[0].starts_with(
        "GET /oauth/request_token?oauth_callback=http%3A%2F%2Flocalhost%3A6414%2F HTTP/1.1\r\n"
    ));
    let first = header(&requests[0], "authorization").unwrap();
    assert!(first.contains(&format!(
        "oauth_consumer_key=\"{}\"",
        crate::tumblr::DEFAULT_API_KEY
    )));
    assert!(!first.contains("oauth_token="));
    verify_oauth1(&requests[0], &root, crate::tumblr::DEFAULT_API_SECRET, "");
    assert!(
        requests[1]
            .starts_with("GET /oauth/access_token?oauth_token=REQ&oauth_verifier=VER HTTP/1.1\r\n")
    );
    verify_oauth1(
        &requests[1],
        &root,
        crate::tumblr::DEFAULT_API_SECRET,
        "REQSECRET",
    );
    for request in &requests {
        assert!(!request.contains(crate::tumblr::DEFAULT_API_SECRET));
    }

    let key = format!(
        "gallery_dl.oauth._token_cache-('tumblr', '{}')",
        crate::tumblr::DEFAULT_API_KEY
    );
    assert_eq!(
        cache.get(&key).await.unwrap(),
        Some(json!(["ACC", "ACCSECRET"]))
    );
    assert!(text(&output).contains("These values have been cached"));
}

#[tokio::test]
async fn oauth1_rejects_mismatched_tokens_and_flickr_needs_a_key() {
    let (root, server) = serve(vec![reply(
        "200 OK",
        "text/plain",
        "oauth_token=REQ&oauth_token_secret=S",
    )]);
    let seen = Arc::new(Mutex::new(None));
    let redirect: Redirect = Arc::new(|_| "oauth_token=OTHER&oauth_verifier=V".into());
    let configured = settings(&[
        ("smugmug", "api-key", json!("custom")),
        ("smugmug", "api-secret", json!("csecret")),
    ]);
    let (flow, _) = flow(configured, None, &root);
    let error = flow
        .with_browser(browser(redirect, seen.clone()))
        .run("smugmug")
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "OAuth response does not match the request token"
    );
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /services/oauth/1.0a/getRequestToken?oauth_callback="));
    assert!(
        header(&requests[0], "authorization")
            .unwrap()
            .contains("oauth_consumer_key=\"custom\"")
    );
    verify_oauth1(&requests[0], &root, "csecret", "");

    let (flow, _) = self::flow(settings(&[]), None, &root);
    assert_eq!(
        flow.run("flickr").await.unwrap_err().to_string(),
        "oauth:flickr requires extractor.flickr.api-key and api-secret"
    );
}

#[tokio::test]
async fn pixiv_pkce_code_is_exchanged_for_a_cached_refresh_token() {
    let (root, server) = serve(vec![json_reply(
        json!({"access_token": "PAT", "refresh_token": "PRT", "user": {"id": "1"}}),
    )]);
    let (_directory, cache) = temp_cache();
    let opened = Arc::new(Mutex::new(None::<Url>));
    let configured = settings(&[("pixiv", "username", json!("alice"))]);
    let (flow, output) = flow(configured, Some(cache.clone()), &root);
    let recorded = opened.clone();
    flow.with_browser(move |url, _| {
        *recorded.lock().unwrap() = Some(url.clone());
        false
    })
    .with_input(|prompt| {
        assert_eq!(prompt, "code: ");
        Ok("https://app-api.pixiv.net/callback?state=x&code=THECODE\n".into())
    })
    .run("pixiv")
    .await
    .unwrap();

    let url = opened.lock().unwrap().clone().unwrap();
    assert_eq!(url.path(), "/web/v1/login");
    let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["client"], "pixiv-android");

    let request = &server.join().unwrap()[0];
    assert!(request.starts_with("POST /auth/token HTTP/1.1\r\n"));
    assert_eq!(header(request, "user-agent"), Some(PIXIV_USER_AGENT));
    let form = body(request);
    assert_eq!(form["code"], "THECODE");
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_id"], crate::pixiv::CLIENT_ID);
    assert_eq!(
        pkce_challenge(&form["code_verifier"]),
        params["code_challenge"]
    );
    assert_eq!(
        cache
            .get_str("gallery_dl.extractor.pixiv._refresh_token_cache-alice")
            .await
            .unwrap()
            .as_deref(),
        Some("PRT")
    );
    let printed = text(&output);
    assert!(printed.contains("4) Copy its 'code' query parameter"));
    assert!(printed.contains("\nYour 'refresh-token' is\n\nPRT\n\n"));
}

#[tokio::test]
async fn pixiv_expired_codes_and_unavailable_caches() {
    let (root, server) = serve(vec![reply(
        "400 Bad Request",
        "application/json",
        &json!({"error": "invalid_grant"}).to_string(),
    )]);
    let directory = tempfile::tempdir().unwrap();
    // A cache in a missing directory cannot be written.
    let cache = Cache::at(directory.path().join("missing").join("cache.sqlite3"));
    let (flow, output) = flow(settings(&[]), Some(cache), &root);
    let error = flow
        .with_browser(|_, _| false)
        .with_input(|_| Ok("CODE".into()))
        .run("pixiv")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Pixiv rejected the authorization code");
    assert!(text(&output).ends_with("'code' expired, try again\n\n"));
    server.join().unwrap();
}

#[tokio::test]
async fn listener_binds_loopback_and_ignores_malformed_requests() {
    let (flow, _) = flow(settings(&[]), None, &Url::parse("http://x/").unwrap());
    let listener = flow.listen().await.unwrap();
    let address = listener.local_addr().unwrap();
    assert!(address.ip().is_loopback());
    let task = tokio::spawn(async move {
        let callback = flow.recv(&listener, &["state", "code"]).await.unwrap();
        callback.params
    });
    let address_clone = address;
    let responses = tokio::task::spawn_blocking(move || {
        // A POST, an unrelated GET and garbage are all answered with 404.
        let mut post = std::net::TcpStream::connect(address_clone).unwrap();
        post.write_all(b"POST /?code=x HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        post.read_to_string(&mut response).unwrap();
        let mut garbage = std::net::TcpStream::connect(address_clone).unwrap();
        garbage.write_all(b"\xff\xfe\r\n\r\n").unwrap();
        let mut other = String::new();
        garbage.read_to_string(&mut other).unwrap();
        let unrelated = get(address_clone, "/?foo=bar");
        let mut valid = std::net::TcpStream::connect(address_clone).unwrap();
        valid
            .write_all(b"GET /?state=s&code=c%2B1 HTTP/1.1\r\n\r\n")
            .unwrap();
        (response, other, unrelated)
    })
    .await
    .unwrap();
    assert!(responses.0.starts_with("HTTP/1.1 404 "));
    assert!(responses.1.starts_with("HTTP/1.1 404 "));
    assert!(responses.2.starts_with("HTTP/1.1 404 "));
    let params = task.await.unwrap();
    assert_eq!(params["state"], "s");
    assert_eq!(params["code"], "c+1");
}

#[tokio::test]
async fn oauth1_settings_resolve_configured_or_cached_tokens() {
    let parse = |value: Value| {
        let map = value.as_object().unwrap().clone();
        OAuth1Settings::parse("Site", |key| map.get(key)).map_err(|e| e.to_string())
    };
    let (_directory, cache) = temp_cache();
    assert_eq!(
        token_cache_key("smugmug", "KEY"),
        "gallery_dl.oauth._token_cache-('smugmug', 'KEY')"
    );
    cache
        .set(
            &token_cache_key("smugmug", "KEY"),
            &json!(["cached-token", "cached-secret"]),
            None,
        )
        .await
        .unwrap();

    // Unset and "cache" tokens read the pair cached for the API key.
    for settings in [
        json!({}),
        json!({"access-token": "cache", "access-token-secret": "x"}),
    ] {
        let resolved = parse(settings)
            .unwrap()
            .resolve("smugmug", "KEY", "default-secret", Some(&cache))
            .await
            .unwrap();
        assert_eq!(
            resolved,
            OAuth1Credentials {
                consumer_key: "KEY".into(),
                consumer_secret: "default-secret".into(),
                token: "cached-token".into(),
                token_secret: "cached-secret".into(),
            }
        );
        assert_eq!(format!("{resolved:?}"), "OAuth1Credentials { .. }");
    }
    // Another key, another site or no cache: no pair, so no signing.
    let settings = parse(json!({})).unwrap();
    assert!(
        settings
            .resolve("smugmug", "OTHER", "s", Some(&cache))
            .await
            .is_none()
    );
    assert!(
        settings
            .resolve("flickr", "KEY", "s", Some(&cache))
            .await
            .is_none()
    );
    assert!(
        settings
            .resolve("smugmug", "KEY", "s", None)
            .await
            .is_none()
    );
    // Configured tokens skip the cache; the configured secret wins.
    let settings = parse(json!({"api-secret": "custom", "access-token": "tok",
        "access-token-secret": "tsecret"}))
    .unwrap();
    let resolved = settings
        .resolve("smugmug", "KEY", "default-secret", Some(&cache))
        .await
        .unwrap();
    assert_eq!(
        (resolved.consumer_secret.as_str(), resolved.token.as_str()),
        ("custom", "tok")
    );
    let debug = format!("{settings:?}");
    assert!(!debug.contains("custom") && !debug.contains("tok\"") && !debug.contains("tsecret"));
    assert!(settings.resolve("smugmug", "KEY", "", None).await.is_some());
    // Any empty part disables signing: an empty key, an empty default or
    // configured secret, `access-token: false`/"" or a missing secret.
    assert!(settings.resolve("smugmug", "", "d", None).await.is_none());
    for value in [
        json!({"api-secret": "", "access-token": "t", "access-token-secret": "s"}),
        json!({"access-token": false, "access-token-secret": "s"}),
        json!({"access-token": "", "access-token-secret": "s"}),
        json!({"access-token": "t"}),
    ] {
        let settings = parse(value).unwrap();
        assert!(
            settings
                .resolve("smugmug", "KEY", "d", Some(&cache))
                .await
                .is_none()
        );
    }
    assert!(
        parse(json!({"access-token": "t", "access-token-secret": "s"}))
            .unwrap()
            .resolve("flickr", "KEY", "", None)
            .await
            .is_none()
    );
    // Invalid values are rejected without echoing them.
    for bad in [
        json!(1),
        json!("has space"),
        json!(true),
        json!("x".repeat(1025)),
    ] {
        let error = parse(json!({"access-token-secret": bad})).unwrap_err();
        assert_eq!(error, "invalid Site setting: access-token-secret");
    }
    // Malformed cache entries count as empty.
    cache
        .set(
            &token_cache_key("smugmug", "BAD"),
            &json!(["only-one"]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(cached_tokens(&cache, "smugmug", "BAD").await, (None, None));
}

#[test]
fn signed_request_headers_validate() {
    let credentials = OAuth1Credentials {
        consumer_key: "ck".into(),
        consumer_secret: "c secret".into(),
        token: "tok".into(),
        token_secret: "t/secret".into(),
    };
    let root = Url::parse("http://127.0.0.1:1/").unwrap();
    let url = root.join("api/v2/album/x?_expand=User&a=b%20c").unwrap();
    let header = credentials.header("GET", &url).unwrap();
    assert!(header.is_sensitive());
    let request = format!(
        "GET /api/v2/album/x?_expand=User&a=b%20c HTTP/1.1\r\nauthorization: {}\r\n\r\n",
        header.to_str().unwrap()
    );
    let fields = verify_signed_request(&request, &root, "c secret", "t/secret");
    assert_eq!(fields["oauth_consumer_key"], "ck");
    assert_eq!(fields["oauth_token"], "tok");
}
