//! Upstream `PixivMeExtractor` (`pixiv.me/NAME` profile links, resolved by
//! their redirect and queued as a user) and `PixivUnlistedExtractor`
//! (`artworks/unlisted/KEY`, read through the web AJAX API like upstream's
//! sanity workaround). Neither needs the App API by itself.
use super::{Pixiv, Target, WorkFailure, backoff, valid_id};
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{StatusCode, header};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use url::Url;

impl Pixiv {
    pub(super) fn me_events(&self, name: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            tokio::time::timeout(self.timeout, self.resolve_me(&name))
                .await
                .map_err(|_| error("native Pixiv extraction timed out"))?
        })
        .map(move |result| match result {
            Ok(user) => crate::event::child_job(
                ChildMetadata::from_raw("pixiv", "me", &Map::new()),
                move || self.events(&user),
            ),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// Upstream `request_location(url, notfound="user")`: the redirect target
    /// of a `pixiv.me` link, which must be a Pixiv user profile.
    async fn resolve_me(&self, name: &str) -> Result<Url> {
        let endpoint = self
            .me_root
            .join(name)
            .map_err(|_| error("invalid pixiv.me link"))?;
        let unresolved = || error("pixiv.me link did not resolve to a Pixiv user");
        for attempt in 0..=self.retries {
            self.rate_limits.wait(&endpoint, self.timeout).await?;
            let response = match tokio::time::timeout(
                self.request_timeout,
                self.client.get(endpoint.clone()).send(),
            )
            .await
            {
                Ok(Ok(response)) => response,
                _ if attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                _ => return Err(error("pixiv.me request failed; check network access")),
            };
            let status = response.status();
            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && attempt < self.retries
            {
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            if status == StatusCode::NOT_FOUND {
                return Err(error("Pixiv user not found"));
            }
            if !status.is_redirection() {
                return Err(unresolved());
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| endpoint.join(value).ok())
                .ok_or_else(unresolved)?;
            let bare = matches!(
                super::segments(&location).as_slice(),
                ["users", _] | ["en", "users", _]
            );
            return match super::target(&location) {
                Ok(Target::User { .. }) if bare => Ok(location),
                _ => Err(unresolved()),
            };
        }
        Err(unresolved())
    }

    pub(super) fn unlisted_events(&self, key: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            let body = self
                .request_ajax(&format!("/illust/unlisted/{key}"))
                .await
                .ok_or_else(|| error("Pixiv unlisted work not found or unavailable"))?;
            let id = match body.get("illustId") {
                Some(Value::String(id)) => id.parse::<u64>().ok(),
                Some(Value::Number(id)) => id.as_u64(),
                _ => None,
            }
            .map(|id| id.to_string())
            .filter(|id| valid_id(id))
            .ok_or_else(|| error("Pixiv returned an invalid unlisted work ID"))?;
            // Upstream `works()`: a placeholder whose ID is replaced by the
            // body's `illustId`, keeping the key as `id_unlisted`.
            let mut work = json!({
                "id": id.parse::<u64>().expect("validated ID"),
                "id_unlisted": key,
                "user": {"id": 1},
            });
            let mut media = self.recover_ajax_work(&mut work, &body).await?;
            if media.is_empty() {
                return Err(error("Pixiv unlisted work has no downloadable image"));
            }
            self.enrich_files(&mut work, &mut media)
                .await
                .map_err(WorkFailure::into_error)?;
            let extra = Arc::new(Map::from_iter([("id_unlisted".to_owned(), key.into())]));
            for file in &mut media {
                if let Some(metadata) = &mut file.metadata {
                    metadata.extra_keywords = Some(extra.clone());
                }
            }
            Ok::<_, crate::Error>(stream::iter(
                self.finish(media).into_iter().map(Ok).collect::<Vec<_>>(),
            ))
        })
        .map(|result| match result {
            Ok(events) => events.boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{pixiv, response, serve};
    use super::*;
    use crate::extract::Media;

    fn files(events: &[ExtractionEvent]) -> Vec<&Media> {
        events
            .iter()
            .filter_map(|event| match event {
                ExtractionEvent::Media(media) => Some(&**media),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn classifies_me_unlisted_and_legacy_work_urls() {
        let t = |raw: &str| super::super::target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://pixiv.me/some_user").unwrap(),
            Target::Me("some_user".into())
        );
        assert_eq!(
            t("https://www.pixiv.net/en/artworks/unlisted/a1b2c3d4e5f6g7h8i9j0").unwrap(),
            Target::Unlisted("a1b2c3d4e5f6g7h8i9j0".into())
        );
        let work = Target::Illust("12345".into());
        for raw in [
            "https://touch.pixiv.net/artworks/12345",
            "https://www.phixiv.net/en/artworks/12345",
            "https://phixiv.net/artworks/12345",
            "https://i.pximg.net/img-original/img/2024/01/02/03/04/05/12345_p0.png",
            "https://i.pximg.net/c/250x250_80_a2/img-master/img/2024/01/02/03/04/05/12345_p0_square1200.jpg",
            "https://i1.pixiv.net/img-inf/img/2013/04/07/02/01/34/12345_s.jpg",
            "https://i2.pixiv.net/img20/img/some_user/12345.jpg",
            "https://img18.pixiv.net/img/some_user/12345.jpg",
            "https://www.pixiv.net/i/12345",
        ] {
            assert_eq!(t(raw).unwrap(), work, "{raw}");
            assert_eq!(
                crate::extract::social_site(&Url::parse(raw).unwrap()),
                Some("pixiv"),
                "{raw}"
            );
        }
        assert_eq!(
            t("https://touch.pixiv.net/users/9").unwrap(),
            Target::User {
                id: "9".into(),
                kinds: vec![super::super::Kind::Illust],
                tag: None
            }
        );
        for raw in [
            "https://pixiv.me/",
            "https://pixiv.me/a/b",
            "https://www.pixiv.net/artworks/unlisted/",
            "https://www.pixiv.net/artworks/unlisted/a-b",
            "https://i.pximg.net/img-original/img/2024/01/02/03/04/12345_p0.png",
            "https://i.pximg.net/img-original/img/24/01/02/03/04/05/12345_p0.png",
            "https://i.pximg.net/user-profile/img/2024/01/02/03/04/05/12345.png",
            "https://img.pixiv.net.evil.example/img/x/12345.jpg",
            "https://www.pixiv.net/i/abc",
            "https://evilphixiv.net/artworks/1",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn me_links_resolve_their_redirect_and_queue_the_user() {
        let (root, server) = serve(vec![
            response("302 Found\r\nLocation: https://www.pixiv.net/users/9", b""),
            super::super::tests::token("API"),
            response(
                "200 OK",
                json!({"illusts": [], "next_url": null})
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let native = pixiv(root.clone()).with_me_root(root);
        let events = native
            .events(&Url::parse("https://pixiv.me/some_user").unwrap())
            .collect::<Vec<_>>()
            .await;
        let scopes: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Ok(ExtractionEvent::BeginChild(scope)) => Some(scope.metadata.subcategory()),
                _ => None,
            })
            .collect();
        // `me` queues the profile, which dispatches its default artworks child.
        assert_eq!(scopes, ["me", "user"]);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /some_user "));
        assert!(!requests[0].contains("refresh-secret"));
        assert!(requests[2].starts_with("GET /v1/user/illusts?user_id=9 "));

        for reply in [
            response(
                "302 Found\r\nLocation: https://www.pixiv.net/artworks/1",
                b"",
            ),
            response("302 Found\r\nLocation: https://evil.example/users/9", b""),
            response("200 OK", b"<html>"),
            response("404 Not Found", b""),
        ] {
            let (root, server) = serve(vec![reply]);
            let native = pixiv(root.clone()).with_me_root(root);
            let events = native
                .events(&Url::parse("https://pixiv.me/some_user").unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(events.len(), 1);
            assert!(events[0].is_err());
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn unlisted_works_read_the_ajax_body_without_app_api_credentials() {
        let body = json!({"error": false, "body": {
            "illustId": "12345", "illustTitle": "Hidden", "createDate": "2024-01-02T03:04:05+00:00",
            "urls": {"original": "https://i.pximg.net/img-original/img/2024/01/02/03/04/05/12345_p0.png"},
            "width": 800, "height": 600, "sl": 2, "viewCount": 1, "commentCount": 0,
            "bookmarkCount": 0, "restrict": 0, "xRestrict": 0, "aiType": 1, "bookStyle": 0,
            "userId": "9", "userAccount": "artist", "userName": "Artist",
            "tags": {"tags": [{"tag": "猫", "translation": {"en": "cat"}}]},
            "illustComment": "", "pageCount": 2}});
        let (root, server) = serve(vec![response("200 OK", body.to_string().as_bytes())]);
        let native = pixiv(root.clone())
            .with_web_root(root)
            .with_refresh_token(None);
        let events = native
            .events(&Url::parse("https://www.pixiv.net/artworks/unlisted/abcDEF123").unwrap())
            .map(|event| event.unwrap())
            .collect::<Vec<_>>()
            .await;
        let media = files(&events);
        assert_eq!(media.len(), 2);
        let metadata = media[1].metadata.as_ref().unwrap();
        assert_eq!(
            (
                metadata.post_id.as_str(),
                metadata.media_id.as_str(),
                media[1].filename.as_str()
            ),
            ("12345", "12345_p1", "12345_p1.png")
        );
        assert_eq!(
            media[1].url.as_str(),
            "https://i.pximg.net/img-original/img/2024/01/02/03/04/05/12345_p1.png"
        );
        assert_eq!(metadata.username.as_deref(), Some("artist"));
        assert_eq!(
            metadata.extra_keywords.as_ref().unwrap()["id_unlisted"],
            "abcDEF123"
        );
        // The archive identity is the work's real ID, like a listed work.
        use sha2::{Digest, Sha256};
        assert_eq!(
            media[1].archive_key().unwrap(),
            format!(
                "pixiv:v1:{:x}",
                Sha256::digest(json!(["12345", "12345_p1", "png"]).to_string().as_bytes())
            )
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /ajax/illust/unlisted/abcDEF123 "));

        let (root, server) = serve(vec![response(
            "200 OK",
            json!({"error": true, "message": "x", "body": []})
                .to_string()
                .as_bytes(),
        )]);
        let native = pixiv(root.clone()).with_web_root(root);
        let events = native
            .events(&Url::parse("https://www.pixiv.net/artworks/unlisted/abc").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
